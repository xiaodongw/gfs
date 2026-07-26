# XVFS Implementation Plan

Status: Draft; core implementation choices accepted  
Companion: [DESIGN.md](DESIGN.md)

## 1. Delivery strategy

Build the smallest vertical slice that tests the product hypothesis:

> A hosted agent can inspect and modify a large monorepo with materially lower
> startup time, network transfer, and local disk than a clone, without losing task
> correctness.

Do not begin by writing a complete Git server. First prove pinned snapshot access,
lazy FUSE reads, remote search, overlay-aware results, and patch export. Use
libgit2 through `git2-rs` for repository operations, `cberner/fuser` for the Linux
filesystem, and stock `git upload-pack` behind the Rust smart-HTTP gateway for
clone/fetch compatibility. The direct snapshot/blob API, not `upload-pack`, serves
FUSE file reads.

Two things that look like polish are on the critical path and are treated as such
below. A mounted commit must survive upstream ref churn, so retention leases land in
M1 rather than with the rest of garbage collection in M7. And a workspace with no
usable `.git` is not a workspace an agent can work in, so the Git-command surface is
an M0 decision with an M2 implementation, not a late compatibility chore.

Suggested staffing:

- two Rust systems engineers for the server/client path;
- one search/storage engineer, joining at M0 to own the M0.4 spike and continuing
  through M4;
- part-time infrastructure, security, and agent-platform support.

Expected elapsed time is roughly 14–18 weeks to a controlled pilot with three
engineers, followed by another 8–16 weeks of hardening for production.

That figure assumes overlap, and the milestone durations below do not sum to it.
Run sequentially, M0 through M6 is 19–26 weeks. The 14–18 week path requires:

- the two systems engineers on the critical path M0 → M1 → M2 → M3 → M6;
- the search engineer on M0.4 and then M4 in parallel with M2 and M3, integrating
  with the client during M3;
- M5 (Git smart HTTP) overlapping M3/M4 when M0.5 selects the synthesized `.git`,
  because that client path does not depend on the gateway;
- no serialization on the M0 decisions, which is why M0 is staffed with everyone.

If M0.5 selects the partial-clone `.git`, the minimum M5 upload-pack/promisor scope
becomes a predecessor of M2 and the milestone graph changes to M0 → M1 → M5-minimum
→ M2. That path is not described as parallel and should use the sequential estimate.
The sequential estimate also applies if the search engineer joins late. The range
depends heavily on FUSE deployment, push support, search-index scale, and the POSIX
behavior required by real builds.

## 2. Milestones

| Milestone | Outcome | Exit gate |
| --- | --- | --- |
| M0: feasibility ✅ | Highest-risk assumptions measured | **Complete — conditional go**, see [go/no-go](../spikes/reports/m0-go-no-go.md) |
| M1: repository API ✅ | Exact revision/tree/file access, retained for the life of a mount | **Complete**, see [M1 report](reports/m1-completion.md) |
| M2: read-only mount | Lazy snapshot is usable as a workspace, `.git` surface included | Representative read/build smoke tests pass |
| M3: writable workspace | Crash-safe overlay and patch export | Mutation model and recovery tests pass |
| M4: agent search | Search does not hydrate base, sees edits, and separates execution status from scoped coverage | Results match the supported materialized `rg` corpus |
| M5: Git compatibility | Stock Git clone/fetch works | Version/protocol matrix passes |
| M6: hosted pilot | End-to-end jobs run safely | Performance and reliability gates met |
| M7: production | Scaled, operable, supportable service | SLO/security/DR review passes |
| M8: native commit/push | Optional direct Git write workflow | CAS and interoperability tests pass |

M8 can move before M7 if direct pushes are required for the pilot. Otherwise patch
export keeps the critical path smaller.

## 3. M0 — Feasibility and architecture spikes ✅ COMPLETE

Duration: 2–3 weeks, run concurrently across the whole team

> **Status: complete, 2026-07-26. Recommendation: conditional go.**
>
> Spike code is in [`spikes/`](../spikes/), reports in
> [`spikes/reports/`](../spikes/reports/), decisions as ADRs in
> [`docs/adr/`](adr/), and the clone baseline in
> [`benchmarks/baseline.md`](../benchmarks/baseline.md).
>
> | Sub-milestone | Status | Deliverable |
> | --- | --- | --- |
> | M0.1 workload baseline | ✅ | [`benchmarks/baseline.md`](../benchmarks/baseline.md) |
> | M0.2 FUSE deployment | ⚠️ met for WSL2 + Docker; **Kubernetes unmeasured** | [ADR 0003](adr/0003-fuse-deployment-model.md) |
> | M0.3 Git integration | ✅ | [ADR 0001](adr/0001-git-integration.md), [ADR 0002](adr/0002-git-object-authorization-boundary.md) |
> | M0.4 search representation | ✅ | [ADR 0004](adr/0004-search-representation.md) |
> | M0.5 Git-command surface | ✅ | [ADR 0005](adr/0005-git-command-surface.md) |
> | M0.6 product decisions | ⚠️ two items need product input | [ADR 0006](adr/0006-mvp-boundary-and-policies.md) |
>
> Four findings **contradicted** the design and changed it: SHA-256 is
> unreachable through `git2-rs` rather than merely experimental; hiding
> `refs/xvfs/` prevents discovery but not access; the specified synthesized
> `.git` contents do not form a repository at all; and `git ls-files`/`git diff`
> against that surface return empty with exit 0 instead of failing visibly,
> which makes the shim a correctness requirement.
>
> The `.git` decision went to the synthesized surface, so **the M2 → M5
> dependency does not invert** and the milestone graph below is unchanged.
>
> Two conditions carry forward. The first is now **deliberately deferred until the
> prototype works locally** ([ADR 0003 amendment](adr/0003-fuse-deployment-model.md));
> it stops being deferrable before M6.1. Re-run
> [`spikes/fuse-probe/deployment-matrix.sh`](../spikes/fuse-probe/deployment-matrix.sh)
> on the real hosted runner, and replace the public stand-in corpus in
> [`spikes/corpus/corpus.conf`](../spikes/corpus/corpus.conf) with the real
> target monorepos.

### M0.1 Workload and baseline

- Select at least two representative monorepos, including one worst-case repository.
- Record repository history size, tip file count, tree count, unique blob count,
  language mix, large files, submodules, LFS, non-UTF-8 paths, and symlinks.
- Select 20–50 real or replayable agent tasks with known expected outcomes.
- Benchmark full clone, shallow clone, partial clone (`blob:none` and `tree:0` where
  applicable), sparse checkout, and warm host-cache variants.
- Capture startup time, bytes transferred, disk used, file-open count, search time,
  build time, and final task correctness.

Deliverable: `benchmarks/baseline.md` plus reproducible scripts and machine profile.

### M0.2 FUSE deployment spike

- Implement a minimal read-only `fuser` filesystem with one remote-backed file.
- Test direct Docker, the intended hosted runner, and Kubernetes if applicable.
- Measure privilege requirements, cancellation, unmount behavior, daemon death,
  kernel caching, request concurrency, and container teardown.
- Decide between direct mount, host daemon, and CSI node plugin.
- Verify that a mount can be safely bind-mounted into the unprivileged agent job.

Exit: a documented deployment path works in the actual hosted environment.

### M0.3 Git integration validation

- Audit `git2-rs`/libgit2 behavior for bare repositories, object lookup, ref
  resolution, byte paths, tree diff, pack generation, object creation, transactions,
  repository formats, and SHA-256.
- Establish the supported-repository-format boundary explicitly: confirm that
  libgit2 cannot read `reftable` repositories, determine the state of its
  experimental SHA-256 support and what build configuration it needs, and decide
  whether unsupported formats are rejected at ingest or converted. Record the
  consequence for the pre-production SHA-256 commitment.
- Implement format detection so an unsupported mirror fails at creation rather than
  serving a partial view.
- Build the `GitRepository` trait and a libgit2-backed proof of concept.
- Define the blocking-worker and request-local handle model around libgit2.
- Proxy smart HTTP v2 to sandboxed stock `git upload-pack` and run clone/fetch,
  shallow-clone, and partial-clone smoke tests.
- Validate the exact GET advertisement and POST stateless-RPC subprocess contracts,
  including the v0/v1 service preamble and the preamble-free v2 advertisement.
- Determine and freeze the partial-clone filter policy. Confirm that
  `uploadpack.allowFilter` and explicit `uploadpackfilter.<filter>.allow` controls
  expose only the required filter families, while gateway request validation
  restricts the exact initial form to `blob:none` and unadvertised-object wants
  remain disabled.
- Pin supported libgit2, `git2-rs`, and stock Git versions and record their licenses
  and packaging requirements.
- Record the accepted integration in an architecture decision record.

Exit: libgit2 and `upload-pack` expose the same refs and objects for the fixture
matrix, and their deployment/runtime boundary is documented.

### M0.4 Search representation spike

- Build blob-key assignment, trigram posting lists, a snapshot Roaring bitmap, and a
  reverse path table for the largest test snapshot.
- Compare index size/build time/query time with a simple per-snapshot Tantivy index.
- Test literals, Unicode, regexes with and without required literals, common tokens,
  repeated blobs, generated/minified files, and binary detection.
- Prototype segment-local bitmap filtering if Tantivy token search is required.
- Measure full and first-parent incremental manifest construction.
- Measure steady-state manifest storage, not just build cost: manifest bytes per
  snapshot multiplied by the number of concurrently retained snapshots under a
  plausible branch and arbitrary-commit workload. This number, not index build time,
  decides whether on-demand search for arbitrary commits is affordable.
- Measure how much of the index and manifest set is binary or oversized and
  therefore excluded, so the completeness contract has real numbers behind it.
- Prototype the two-dimensional terminal result: execution status versus coverage
  exclusions within the requested path scope. Measure how often a strict exhaustive
  mode would be needed and ensure ordinary repositories containing binaries do not
  make every normal query fail.

Exit: demonstrate correct results and acceptable projected storage/query cost, with
retained-snapshot storage projected over the pilot's expected commit churn.

### M0.5 Git-command surface inside the mount

The design's default is a synthesized read-only `.git` plus a `git` shim. This spike
tests the alternative with numbers instead of intuition.

- Inventory what the pilot's agents and build tooling actually invoke: capture
  `git` command frequency and argument shapes from real agent transcripts, and grep
  the target repositories' build and CI configuration for repository-root detection
  and `git` invocations.
- Freeze the exact shim grammar—global options, subcommands, flags, pathspecs, output
  modes, and exit codes. The candidate synthesized scope is `status`, `diff`,
  selected `rev-parse`, `ls-files`, `show HEAD:<path>`, and bounded `log -1`, not
  arbitrary forms of those subcommands.
- Build a shallow, blobless partial clone of the worst-case repository against a
  mounted or simulated tree and measure first and subsequent `git status`: wall
  time, FUSE metadata operations, bytes transferred, and index size on disk.
- Measure the same for `git diff` with a small edit set, and record what
  `git checkout`/`git reset --hard` would hydrate.
- Prototype the synthesized `.git` and confirm which tools it satisfies and which
  fail, and that they fail visibly rather than reporting a wrong tree state.
- Verify ownership and `safe.directory` behavior for a bind-mounted workspace.
- Decide: synthesized only, partial clone, or synthesized with partial clone behind
  a mount option. Record it in an architecture decision record.
- If partial clone wins, record the hard dependency from its M2 implementation to
  the minimum M5 smart-HTTP/promisor milestone.

Exit: a decision backed by measured `git status` cost on the worst-case repository,
and a list of tools that the chosen option does not satisfy.

### M0.6 Product decisions

- Answer the open questions in the design.
- Freeze MVP compatibility boundaries and performance gates.
- Define the server/client API versioning policy and repository path semantics.
- Threat-model the proposed host cache and FUSE privilege boundary.
- Create an initial failure-mode and data-retention policy.
- Fix the mount retention-lease policy: lease lifetime, orphan expiry, and the
  interaction with repository garbage collection. Include heartbeat frequency,
  renewal grace, mount-capability authorization, hiding of `refs/xvfs/`, and
  exclusion of that namespace from mirror/prune refspecs.
- Fix the timestamp policy using a server-cataloged sanitized snapshot time and a
  client-side monotonic overlay clock; include future-dated commits and host clock
  skew in its acceptance cases.

Go/no-go gate: proceed only if lazy mount works on the target platform, search index
storage is viable at steady state, a Git-command surface that the pilot's tooling
accepts has been identified and costed, and projected task savings are meaningful
over partial clone.

## 4. M1 — Repository and snapshot API ✅ COMPLETE

Duration: 3 weeks

> **Status: complete, 2026-07-26.** All five exit criteria met; see the
> [M1 completion report](reports/m1-completion.md).
>
> Code is in [`crates/`](../crates/), the gate is
> [`scripts/check.sh`](../scripts/check.sh), and the local stack is
> [`scripts/dev-stack.sh`](../scripts/dev-stack.sh).
>
> | Sub-milestone | Status | Deliverable |
> | --- | --- | --- |
> | M1.1 workspace and foundation | ✅ | `xvfs-*` workspace, `scripts/check.sh`, `scripts/dev-stack.sh` |
> | M1.2 catalog and retention leases | ✅ | `xvfs-server/src/catalog/`, `mounts.rs`, `mirror.rs` |
> | M1.3 Git object service | ✅ | `xvfs-git` |
> | M1.4 snapshot and blob APIs | ✅ | `xvfs-proto`, `xvfs-server/src/service/` |
> | M1.5 authentication and authorization | ⚠️ OIDC is a declared seam | `xvfs-server/src/auth/`, `audit.rs` |
>
> Criterion 1 measured: 1,000,002 entries paged across 1000 directories in 13.2 s,
> then one file read, with no client-side repository.
>
> Four findings changed something rather than confirming it: `rustfmt.toml`'s BOM
> silently disabled formatting entirely; `revparse` is too powerful for a service
> boundary, and `main^{tree}` in particular yields a tree where every layer expects
> a commit; hiding `refs/xvfs/` needs *two* spellings, because Git resolves the
> short name `xvfs/mounts/<id>` to a live lease anchor; and lease protection has to
> account for the prune delay, or a mistaken expiry is unrecoverable by definition.
>
> One scope reduction carries forward: **M1.5's OIDC integration is a seam.** No
> provider has been chosen (ADR 0006) and none was reachable, so `Authenticator` is
> a trait with a development static-token verifier. Everything M1.5 gates on is
> provider-independent and implemented.

### M1.1 Workspace and engineering foundation

- Create a Cargo workspace with:
  - `xvfs-types`: object IDs, byte paths, revisions, errors, limits;
  - `xvfs-proto`: Protobuf definitions and generated clients;
  - `xvfs-git`: `GitRepository` abstraction and `git2-rs` implementation;
  - `xvfs-server`: HTTP/gRPC process;
  - `xvfs-search`: indexing and query library;
  - `xvfs-overlay`: overlay state machine;
  - `xvfs-fuse`: filesystem adapter;
  - `xvfs-cli`: user/orchestrator commands;
  - `xvfs-test`: fixtures, fake server, and fault injection.
- Pin a stable Rust toolchain and minimum supported Rust version.
- Add formatting, Clippy, unit/doc tests, dependency audit, license checks, SBOM,
  secret scanning, and reproducible release builds. The license check must assert
  the dependency table in [ADR 0001](adr/0001-git-integration.md) directly:
  scanning crate metadata alone reports this stack as fully permissive and misses
  the statically linked GPL-2.0 libgit2 entirely.
- Establish structured error codes, request IDs, tracing, metrics, and redaction.
- Ship a one-command local development stack: server, seeded fixture repositories of
  several sizes, and a mount, runnable on a laptop without hosted infrastructure.
  Every later milestone depends on this, so it is a deliverable rather than a
  convenience. Include the libgit2 and stock Git version pinning from M0.3 so local
  and CI environments cannot drift.

### M1.2 Repository catalog and lifecycle

- Define repository IDs independently from display names and filesystem paths.
- Implement create/import/mirror, fetch, verify, maintenance, quarantine, and delete
  state machines.
- Store upstream configuration and credential references, not raw secrets.
- Implement webhook and polling ingestion with idempotent ref events.
- Add per-repository locking and reconciliation after partial failure.
- Validate repository format on import and reject what libgit2 cannot serve.
- Implement mount retention leases as a crash-consistent state machine: under the
  repository lock, resolve and authorize the revision, persist `PREPARING`, create
  the reserved ref anchor, persist `ACTIVE`, and only then issue the mount. Reconcile
  partial states after restart, and release on unmount, job cleanup, or expiry. Make
  Git maintenance and garbage collection treat live leases as reachability roots.
- Implement authenticated idempotent renewal, expiry grace, restart recovery, and
  alerts for a live daemon that cannot renew.
- Hide `refs/xvfs/` from upload-pack and ordinary ref enumeration, reject it as a
  user revision, and exclude it from every upstream fetch/prune refspec. Do not use
  unrestricted mirror pruning over internal refs.
- Test that a force push, branch deletion, upstream rebase, or `git gc` cannot make
  a mounted commit unreadable, that an orphaned lease expires, that a live renewed
  lease does not expire, that `ls-remote` cannot see lease refs, and that upstream
  fetch/prune cannot remove them.

Retention leases are in M1 rather than M7 on purpose. Without them, a routine
upstream force push during a pilot job prunes objects out from under a live mount
and every uncached read fails permanently mid-task.

### M1.3 Git object service

- Implement algorithm-generic OID parsing and canonical verification.
- Record a stable sanitized `snapshot_time` when a commit is first cataloged; never
  use an unbounded future committer timestamp as a filesystem clock. Return the
  stored value from revision, mount, and commit metadata APIs.
- Resolve branch, tag, full commit OID, and allowed abbreviation safely.
- Read commits, annotated tags, trees, blobs, modes, and object sizes.
- Implement byte-safe tree traversal, directory pagination, and batch stat.
- Cache decoded immutable trees with bounded memory.
- Reject unreachable or unauthorized object access.

### M1.4 Snapshot and blob APIs

- Define and implement `ResolveRevision`, `GetEntry`, `ListDirectory`,
  `BatchGetEntry`, `GetCommit`, and `PrepareSnapshot`.
- Define and implement atomic `CreateMount`, idempotent `RenewMount`, and
  `ReleaseMount`; return a capability binding subject, repository, commit, mount ID,
  and expiry.
- Implement file-by-revision and immutable blob HTTP endpoints.
- Add ETag, range handling, cancellation, deadlines, response limits, and
  backpressure.
- Ensure branch convenience calls return the resolved commit.
- Add API compatibility/golden tests for Protobuf and JSON errors.

### M1.5 Authentication and authorization

- Integrate the chosen OIDC/workload identity.
- Enforce repository permissions uniformly across Git, RPC, file, and search APIs.
- Add short-lived mount credentials or host-daemon delegation.
- Require the mount capability when a **snapshot, blob, or search** API accesses
  a pinned commit no longer reachable from a currently visible ref. Within those
  APIs, repository access alone must not reach a commit retained for another
  subject's mount.
  This requirement is **scoped to the XVFS APIs and does not extend to the Git
  gateway**. M0.3 measured that stock `upload-pack` serves any object in the
  repository's object database by object ID over protocol v2, regardless of
  `uploadpack.allowAnySHA1InWant`, so a repository reader can always reach a
  lease-retained commit through Git. One bare repository is one authorization
  domain; see [ADR 0002](adr/0002-git-object-authorization-boundary.md). Do not
  write an acceptance test that expects the Git path to deny it.
- Add audit records without logging source content, tokens, or unsafe paths.
- Test confused-deputy, path traversal, unauthorized OID, and stale credential cases.

Exit criteria:

- A test can resolve a revision, list a million-entry snapshot one directory at a
  time, and fetch an individual file without cloning.
- Concurrent ref movement cannot produce mixed-commit responses.
- A leased commit survives a force push, a branch deletion, and a full `git gc`, and
  an expired lease stops protecting it. Revision resolution and lease creation have
  no race window, and a renewed live lease survives past its original TTL.
- Lease refs are absent from all Git advertisements and survive upstream
  fetch/prune operations.
- Unauthorized users cannot infer blob existence through status, timing within a
  defined tolerance, cache, or error differences.

## 5. M2 — Read-only FUSE client

Duration: 3–4 weeks

### M2.1 Mount lifecycle

- Implement `xvfs mount`, `unmount`, `inspect`, and daemon health commands.
- Persist mount identity, repository, pinned commit, snapshot time, lease state, API
  version, and format version.
- Call atomic `CreateMount` so revision resolution, authorization, lease catalog
  write, and ref anchoring complete before the client receives the pinned commit.
  Show that commit in CLI output.
- Handle signals, forced teardown, stale mounts, daemon restart, and job cleanup.
- Renew the retention lease on a heartbeat, recover renewal after daemon restart,
  surface renewal failure before grace expires, and release it on unmount or cleanup.
- Implement the chosen host-daemon/CSI integration skeleton.
- Implement the `.git` surface selected in M0.5. For the default option that means
  a synthesized read-only `HEAD`, `packed-refs`, `config`, and `xvfs.json`, excluded
  from search, status, diff, export, and hydration accounting; verify mount
  ownership and `safe.directory` behavior under the bind-mount.
- Implement `xvfs refresh` for a clean workspace only by creating a second mount
  generation and atomically replacing the bind mount. Do not mutate the pinned base
  under existing long-lived kernel dentries. Keep the old generation and its lease
  until old file and directory handles close, then tear it down. Refuse with a clear
  error when the overlay is non-empty; three-way refresh stays out of scope.

### M2.2 Metadata and inode model

- Implement stable base inode assignment, inode lookup counts, and forget handling.
- Implement `lookup`, `getattr`, `opendir`, `readdirplus`, `releasedir`, `readlink`,
  `access`, and `statfs`.
- Preserve byte paths and Git modes.
- Implement the timestamp rule from the design: base entries report the server's
  stable sanitized snapshot time, and overlay mutations use a monotonic logical
  clock no earlier than the base plus one tick. Test that future-dated commits,
  equal-tick writes, and skewed host clocks still leave every acknowledged local
  edit newer than the base, and that base timestamps are identical across remounts
  and hosts. Clamp explicit overlay timestamp requests to the overlay floor and
  document that exact restoration of an older mtime is outside MVP compatibility.
- Implement `statfs` against the overlay quota rather than the host filesystem.
- Add positive/negative cache TTL and invalidation rules.
- Represent gitlinks and unsupported object modes predictably.

### M2.3 Blob cache and reads

- Implement whole-blob download, temporary-file verification, atomic publication,
  open-handle pinning, and LRU eviction.
- Verify `blob <size>\0<content>` with the repository hash algorithm.
- Implement `open`, `read`, `lseek`, `flush`, `release`, and `mmap` behavior as
  supported by FUSE/kernel. Determine whether writable `MAP_SHARED` is available in
  the target deployment and whether enabling the writeback cache to get it is
  acceptable; record the answer as a compatibility boundary either way.
- Deduplicate concurrent misses for the same OID.
- Add retry policy, timeouts, cancellation, offline cached reads, and clear errno
  mapping.
- Partition cache namespaces by the agreed authorization boundary.

### M2.4 Compatibility tests

- Run a relevant `pjdfstest`/xfstests subset and document intentional deviations.
- Test shell tools, editors, language servers, compilers, archive tools, and build
  systems selected in M0.
- Test huge directories, deep paths, non-UTF-8 names, symlink loops, branch races,
  server loss, slow responses, corrupt blobs, and cache eviction.
- Compare visible snapshot metadata and file bytes to a raw-tree materializer built
  from `git ls-tree` and `git cat-file`, since the mount serves raw blob bytes and a
  normal checkout can still apply built-in `.gitattributes` conversion. Then
  run the same comparison with filters enabled on a repository that uses
  `.gitattributes` and LFS, and record the divergence as documented expected
  behavior rather than a test failure.
- Exercise the `.git` surface and `git` shim: repository-root detection, branch and
  commit reporting, the supported read-only subcommands, and confirmation that
  unsupported subcommands fail with an actionable message instead of a wrong answer.

Exit criteria:

- Cold mount meets the startup/download target.
- Reading selected files transfers only required metadata and blobs.
- The chosen representative read-only build or analysis tasks succeed, with the
  tooling that probes for a repository root working against the `.git` surface.
- Base timestamps are stable across remounts and hosts and do not confuse the
  selected build systems, including with future-dated commits and clock skew.
- Refresh exposes only the old or new mount generation; open old-generation handles
  remain valid until close and no kernel-cached path mixes generations.
- Daemon or server failure does not corrupt the shared cache.

## 6. M3 — Writable overlay and export

Duration: 3–4 weeks

### M3.1 Overlay data model

- Specify the path state machine for base, copied-up, created, deleted, renamed, and
  type-changed entries.
- Specify transaction ordering for journal write, file fsync, directory fsync, and
  atomic rename.
- Add schema versioning, migration, recovery, and consistency checking.
- Implement per-inode/open-handle synchronization and lock ordering.
- Build a pure model for property-based state-machine tests.

### M3.2 Mutation operations

- Implement create, mkdir, write, truncate, chmod executable bit, symlink, unlink,
  rmdir, and rename.
- Optimize `O_TRUNC` to avoid an unnecessary base fetch.
- Merge overlay entries and whiteouts correctly in lookup and readdir.
- Preserve open-file semantics across rename/unlink.
- Define behavior for hard links, xattrs, sparse files, `fallocate`, file locks, and
  unsupported special files; implement only what pilot workloads require.
- Enforce per-job overlay disk quota without endangering existing edits.

### M3.3 Status and diff

- Produce status solely from the journal.
- Generate text diffs, binary change records, symlink changes, mode changes, adds,
  deletes, and renames.
- Fetch base blobs only for changed paths that require a comparison.
- Add deterministic JSON and Git-patch exports with base commit metadata.
- Implement atomic export bundles and checksums.
- Build a verifier that applies an export to a clean checkout and compares trees.
- Back only the exact `git` shim grammar frozen in M0.5: `status`, `diff`, the
  selected `rev-parse` and `ls-files` forms, `show HEAD:<path>`, and bounded
  `log -1` metadata obtained through `GetCommit`. Test every supported flag
  combination against real Git over a materialized checkout, and test that all
  other commands and flags fail with an actionable message rather than hydrating or
  returning an approximation.

### M3.4 Crash and concurrency testing

- Kill the daemon at every journal/file transaction boundary.
- Test disk full, inode exhaustion, permission failure, server loss, cache eviction,
  concurrent writers, rename cycles, and unmount with open files.
- Run the filesystem compatibility suite in writable mode.
- Prove recovery is idempotent and never discards an acknowledged edit.

Exit criteria:

- Random mutation sequences match the reference in-memory filesystem model.
- Export applied to the pinned Git commit produces the same tree as the mounted
  workspace.
- Fault injection meets the no-lost-acknowledged-mutation goal.

## 7. M4 — Revision-aware agent search

Duration: 3–4 weeks

### M4.1 Blob registry and text classification

- Assign stable, repository-scoped blob keys transactionally.
- Detect binary, UTF-8, oversized, generated, and vendored content with recorded,
  configurable policy. Generated and vendored files are classifications, not
  default exclusions; any exclusion must be declared in coverage metadata.
- Decode line boundaries without normalizing returned byte offsets.
- Index each OID once and make ingestion idempotent.
- Rate-limit and sandbox parsing/tokenization.

### M4.2 Snapshot manifests

- Implement full parallel tree-walk manifest construction.
- Implement first-parent incremental path-table and bitmap updates.
- Store forward and reverse path maps plus checksums and format versions.
- Prepare configured branch tips eagerly and arbitrary commits on demand.
- Deduplicate simultaneous preparation requests.
- Implement READY/BUILDING/FAILED state, progress, retry, cancellation, and TTL.
- Garbage-collect manifests no longer retained by refs, jobs, or policy.

### M4.3 Literal and regex index

- Implement trigram extraction, posting creation, compaction, and lookup.
- Intersect candidate postings with snapshot membership before reading blobs.
- Verify matches and return exact path, line, column, snippet, OID, and commit.
- Implement path globs, case sensitivity, context lines, pagination/streaming, and
  deterministic ordering.
- Use bounded regex automata and reject or budget broad scans.
- Add result, time, candidate, bytes-read, and concurrency quotas.
- Implement the completion contract: every successful stream ends with exactly one
  terminal message that separately reports execution status (`COMPLETE` or
  `TRUNCATED`) and policy/index coverage, including eligible-path count, exclusions
  grouped by reason and scope, stop budget, and index generation. Treat EOF, RPC
  failure, or cancellation before that terminal message as a failed search. Test
  that budget exhaustion, policy exclusion, index gaps, and partial backend failure
  cannot reach the client as a plain empty result.

### M4.4 Optional token search

- Define tokenizers for source and prose; keep this an explicit query mode.
- Index blob keys as fast fields in Tantivy.
- Build/cache segment-local snapshot filters and test them across segment merges.
- Define ranking and explain behavior.
- Skip this task if M0 shows literal/regex covers agent workloads.

### M4.5 Overlay-aware client search

- Query the exact pinned commit, never the original branch name.
- Exclude changed, deleted, renamed-from, and type-changed paths from base results.
- Search created, copied-up, and modified local files without contacting the server.
- Bound the local half of the search the same way the server bounds its own: honor
  `.gitignore` and `.git/info/exclude` from the merged workspace, apply the server's
  binary and size classification, and enforce a local time and bytes-read budget.
  Benchmark against an overlay containing a full build tree; `xvfs search` must not
  become slower than the `rg` invocation it replaces. Provide an explicit flag to
  search ignored files.
- Merge context, ordering, limits, and exit codes predictably.
- Surface execution status and coverage separately end to end. Merge server and
  local exclusions into one scoped coverage report, print it on stderr in text mode,
  and expose it as fields in `--json`. Execution truncation, a missing terminal
  message, and transport/backend failure use non-success exit codes. Declared
  coverage exclusions warn by default; `--require-exhaustive` turns any coverage gap
  into failure. Cover this contract in the agent instructions and MCP tool schema.
- Add `--json` and ripgrep-like text output.
- Implement `xvfs-rg` for the selected safe flag subset and fail closed on
  unsupported flags unless explicit hydration is requested.
- Publish agent instructions and an MCP/native tool schema.

### M4.6 Search correctness and performance

- Materialize the same commit and compare XVFS results to `rg` for a large generated
  query corpus within the declared supported corpus and matching semantics.
- Cover CRLF, files without final newline, Unicode, invalid UTF-8 paths, repeated
  blobs, symlinks, binary files, huge lines, regex corner cases, and overlay edits.
- Verify a server search fetches zero blobs into the client cache.
- Benchmark cold/warm branch and arbitrary-commit preparation.
- Fault-inject budget exhaustion, binary/oversized/policy exclusions, index gaps,
  transport loss before the terminal message, and partial backend failure. Assert
  that execution truncation/failure and coverage gaps remain distinguishable and
  that default and `--require-exhaustive` exit behavior matches the contract.

Exit criteria:

- Supported literal/regex results match the documented `rg` semantics.
- Search after edits returns the merged logical workspace.
- Warm search meets the performance target and causes zero base hydration.
- No tested failure or limit produces a result that is indistinguishable from
  "no matches".

## 8. M5 — Git smart HTTP compatibility

Duration: 2–4 weeks

### M5.1 Smart HTTP gateway

- Implement info/refs and upload-pack routes with streaming and backpressure.
- Reproduce `git-http-backend`'s version-dependent framing exactly. For protocol
  v0/v1 info/refs, prepend the `# service=git-upload-pack` pkt-line and flush packet
  because `upload-pack --http-backend-info-refs` does not emit them. For protocol
  v2, do not add that preamble; the response starts with the `version 2` pkt-line
  emitted by upload-pack. Return
  `application/x-git-upload-pack-advertisement` with `Cache-Control: no-cache` for
  the advertisement and `application/x-git-upload-pack-result` for the RPC.
- Accept `Content-Encoding: gzip` request bodies and decompress them under explicit
  output-size and ratio limits before forwarding to the subprocess.
- Pass `Git-Protocol` v2 negotiation correctly.
- Start upload-pack with protected configuration that enables filtering explicitly
  (`uploadpack.allowFilter=true`), disables unselected filter families with
  `uploadpackfilter.<filter>.allow`, hides `refs/xvfs/*`, and leaves arbitrary
  unadvertised object wants disabled. Validate the exact requested filter in the
  gateway, initially allowing only `blob:none`, because Git's configuration can
  permit a filter family more broadly than XVFS policy.
- Disable hooks and unsafe repository/path/environment behavior.
- Add authentication, authorization, request-size, CPU, memory, time, and process
  limits.
- Scrub progress/error output and attach request telemetry.

### M5.2 Protocol feature matrix

- Test byte-exact v0/v1 advertisements with the service preamble and v2
  advertisements beginning with `version 2`, plus v2 `ls-refs`/`fetch`.
- Test thin packs, sideband, shallow fetch, the allow-listed partial-clone filters,
  ref-in-want if selected, and cancellation according to supported scope. Verify
  that filtering is not advertised when disabled, is advertised when the protected
  configuration enables it, and unsupported filters fail closed.
- Test empty, corrupt, alternates-based, SHA-1, and later SHA-256 repositories.
- Test multiple maintained Git client versions on Linux and at least one other OS.
- Verify every clone and fetch result independently, since the server's protocol
  engine is stock Git and cannot serve as its own oracle: run `git fsck` on the
  received repository and compare its resolved trees against the libgit2-backed
  snapshot API and a direct filesystem clone of the bare repository.
- If M0.5 selected the partial-clone `.git`, verify that the gateway works as a
  promisor remote for on-demand blob fetches from inside a mounted workspace.
- Fuzz HTTP routing, headers, repository selection, subprocess setup, and response
  streaming at the Rust trust boundary; send malformed pkt-line inputs through the
  sandbox to verify resource limits and safe failure.

### M5.3 Subprocess and repository isolation

- Invoke `git upload-pack` directly without a shell or user-controlled executable
  path, arguments, working directory, environment, or Git configuration.
- Validate and allow-list `Git-Protocol` values before setting `GIT_PROTOCOL`.
- Disable hooks and unsafe configuration and make the repository read-only to the
  upload-pack process.
- Build the protected upload-pack configuration independently of repository and
  user configuration. Test that `refs/xvfs/*` is absent from v0/v1 advertisements
  and v2 `ls-refs`, and that repository configuration cannot re-enable hidden refs,
  hooks, arbitrary unadvertised wants, or disallowed filters.
- Apply process count, CPU, memory, output, inactivity, and wall-clock limits.
- Propagate cancellation, reap every child, and test disconnects during
  advertisement, negotiation, and pack streaming.
- Bound stderr capture, redact repository paths, and preserve useful diagnostics.
- Coordinate repository maintenance so active libgit2 readers and upload-pack
  processes never observe partially replaced packs.
- Compare ref advertisement, object reachability, and cloned output with the
  libgit2-backed snapshot API across the fixture matrix.

Exit criteria:

- Stock Git clone/fetch and partial-clone tests pass the declared version/feature
  matrix.
- Git traffic cannot bypass the same repository authorization used by XVFS APIs.

## 9. M6 — Hosted-agent pilot

Duration: 3–4 weeks

### M6.1 Orchestrator integration

- Create/mount before the job and unmount/archive after the job.
- Pass repository, revision, job identity, limits, and credentials securely.
- Bind-mount into an unprivileged container with the expected ownership.
- Install CLI, `xvfs-rg`, the `git` shim, agent instructions, and optional MCP tool,
  and verify shim precedence in `PATH` inside the real agent image.
- Collect patch/export as a job artifact and feed it into the existing review/commit
  pipeline.
- Handle cancellation, timeout, node drain, orphan mounts, and cleanup retries,
  including heartbeat renewal, warning before the renewal grace deadline, and
  retention-lease release on every teardown path.
- Resolve `xvfs materialize`: if M0.2 found a target environment that cannot mount,
  implement selective materialization of an explicit path set into a plain directory
  here; if every target environment can mount, remove it from the design rather than
  leaving an unimplemented fallback in the document. Note that this is distinct from
  the pilot's clone fallback in M6.5, which abandons XVFS for the job entirely.

### M6.2 Hydration controls

- Attribute hydration to job, process, path, and operation where the OS permits.
- Add per-job soft/hard budgets and user-visible diagnostics.
- Produce reports showing commands that caused bulk hydration.
- Add prefetch for an explicit path set without scanning the mount.
- Tune metadata/blob concurrency, read-ahead, and cache sizes from real jobs.

### M6.3 Operations

- Add dashboards for mount latency, search latency, cache hit rate, fetched bytes,
  overlay bytes, index lag, errors, and cleanup backlog.
- Add alerts, runbooks, support bundle, health/readiness probes, and safe debug logs.
- Add rolling-upgrade compatibility between client, server, state, and index formats.
- Add backup/rebuild procedures and a disaster-recovery exercise.
- Load-test mount storms, popular-repo hot spots, branch update bursts, and search
  concurrency.

### M6.4 Security and abuse review

- Pen-test repository/path parsing, symlinks, object authorization, signed URLs,
  cache isolation, regex limits, and subprocess sandbox.
- Review FUSE/CSI privileges and agent-to-daemon control socket permissions.
- Add dependency provenance, signed artifacts, SBOM, vulnerability response, and
  release approval.
- Define content/audit retention and deletion propagation.

### M6.5 Pilot evaluation

- Randomly assign selected tasks to XVFS and current clone workflow.
- Compare correctness first, then wall time, bytes, disk, failure rate, and cost.
- Categorize every forced hydration and unsupported filesystem behavior.
- Define rollback and automatically fall back to clone on safe, recognized startup
  failures.

On the statistics: a 20–50 task corpus can only detect large correctness
differences. Treat correctness as a per-task gate rather than a statistical test —
every XVFS failure that the clone workflow did not also produce must be root-caused
and either fixed or accepted explicitly. Compute a confidence interval on the
difference and report it honestly as wide. Reserve statistical claims for the
resource metrics, where per-task measurements are paired and the effect sizes are
large.

Pilot gate:

- zero unexplained correctness regressions, each XVFS-only failure root-caused;
- meaningful median and tail improvement on startup bytes/disk;
- acceptable end-to-end task latency;
- no unresolved high-severity security findings;
- mount cleanup and recovery meet the agreed reliability target.

## 10. M7 — Production hardening and scale

Duration: 8–16 weeks, driven by scale and compliance

### M7.1 Multi-node data plane

- Partition repositories and implement owner routing/rebalancing.
- Move catalog/outbox/jobs to PostgreSQL with tested migrations.
- Store immutable manifests, decoded blobs, and index generations in object storage.
- Add local NVMe admission/eviction and hot-repository replication.
- Implement atomic index-generation publication and replica warmup.
- Prove a complete derived-data rebuild from Git.

### M7.2 Retention and garbage collection

- Extend the M1 mount-lease mechanism to the full reachability-root set: refs,
  active mounts, retained commits, exports, and prepared snapshots, now coordinated
  across nodes rather than within one.
- Expire snapshot manifests and unused decoded blobs without racing readers.
- Coordinate Git maintenance with object readers and active upload-pack processes.
- Report reclaimed bytes, failed deletions, and legal-hold exceptions.
- Test restore and GC interruption.

### M7.3 Availability and performance

- Define regional topology, failover, and data-local routing.
- Add admission control and fair scheduling between repositories/tenants.
- Protect against thundering-herd blob and manifest builds with single-flight work.
- Add circuit breakers and bounded queues.
- Establish SLOs and error budgets from pilot data.
- Capacity-model Git packs, decoded blobs, indexes, egress, CPU, memory, and inode
  usage.

### M7.4 Packaging and lifecycle

- Build signed server containers and client packages.
- Build and qualify the CSI/host-daemon deployment.
- Document install, upgrade, downgrade, cache migration, and uninstall.
- Publish administrator, agent-author, and troubleshooting documentation.
- Establish API/index/state deprecation windows and compatibility tests.

Production gate:

- reliability, security, privacy, disaster recovery, and capacity reviews approve;
- load/soak tests meet SLOs at expected peak plus safety margin;
- client/server rolling upgrade and rollback have been exercised;
- on-call runbooks and ownership are in place.

## 11. M8 — Native commit and push

Duration: 3–5 weeks for a constrained workflow

### M8.1 Commit creation

- Hash created/modified overlay content using the repository algorithm.
- Reuse unchanged trees and rewrite only affected tree paths.
- Create commits with validated author/committer identity and message.
- Support signed commits only after a key-management design.
- Verify the generated commit/tree using both libgit2 and stock `git fsck`.

### M8.2 Server object upload

- Define an authenticated object/pack upload protocol or use receive-pack.
- Quarantine uploads, verify connectivity and object integrity, run policy checks,
  then promote.
- Bound pack size, object count, delta depth, and decompression work.
- Make retries idempotent.

### M8.3 Ref compare-and-swap

- Update only when `expected_old_oid` equals the current allowed ref.
- Return structured conflict information when the branch moved.
- Integrate branch protection, hooks/policy, audit, and ref-change indexing.
- Never automatically force push.

### M8.4 Interoperability

- Clone the resulting branch with stock Git and compare the expected tree.
- Test simultaneous agent writers, retries after ambiguous network failure, branch
  deletion, policy rejection, and index event recovery.
- Design refresh/rebase separately; do not hide conflicts in the push path.

Exit criteria:

- A mounted job can create an ordinary commit and safely advance an allowed branch.
- Ambiguous failures and concurrent updates cannot lose or overwrite another commit.

## 12. Cross-cutting test matrix

Every milestone adds cases to the same automated matrix:

| Axis | Cases |
| --- | --- |
| Git | client versions, protocol versions, SHA-1/SHA-256, shallow/partial, packed/loose, `files` vs rejected `reftable` backend, gzip-encoded request bodies |
| Trees | empty, million files, deep, huge directory, non-UTF-8, mode changes, submodule |
| Files | empty, text, binary, CRLF, huge line, huge blob, symlink, executable |
| Network | latency, loss, retry, timeout, reset, truncated/corrupt response, search EOF before terminal completion |
| Storage | disk full, read-only, inode full, corruption, eviction, restart |
| Concurrency | duplicate fetch, parallel readers/writers, rename/unlink while open |
| Security | unauthorized repo/OID, path traversal, symlink escape, regex abuse, token expiry |
| Lifecycle | crash, forced unmount, orphan cleanup, upgrade, downgrade, GC race, force push and branch deletion under a live mount, lease renewal, grace, expiry, and hidden-ref pruning |
| Agent | raw `rg`, XVFS search, edit-then-search, execution-truncated and coverage-excluded search, strict exhaustive search, build, test, diff/export, exact `git` shim grammar and `.git` probing, ignored-file overlay search |

Correctness oracles:

- a raw-tree materializer built from `git ls-tree` and `git cat-file`, which avoids
  checkout-time `.gitattributes`, `core.autocrlf`, clean/smudge, and LFS conversion;
- `ripgrep` over that raw materialization for the supported searchable corpus, with
  binary, oversized, ignored, and other declared exclusions compared to coverage
  metadata rather than silently omitted;
- an in-memory overlay state model;
- `git apply`/tree comparison for exported patches;
- for protocol behavior, stock Git *clients* across the supported version matrix,
  plus `git fsck` and tree comparison on the cloned result, and a direct filesystem
  clone of the bare repository. The server's `upload-pack` is the implementation and
  cannot be its own oracle;
- real `git status`/`diff` over a materialized checkout with the same edits, for the
  `git` shim.

## 13. Initial backlog ordering

The first executable backlog should be:

1. M0.1–M0.5 spikes in parallel where staffing permits.
2. Architecture review and frozen MVP boundary, including the `.git` decision.
3. Workspace/types/protocol foundation and the local development stack.
4. Exact revision and file API, with mount retention leases.
5. Minimal read-only remote mount, including the chosen `.git` surface.
6. Verified shared blob cache.
7. Overlay state model, then mutations and export.
8. Literal search index, snapshot manifest, and the terminal execution/coverage
   contract.
9. Overlay-aware `xvfs search`, the `git` shim, and agent tool integration.
10. Smart HTTP compatibility matrix.
11. Hosted orchestrator integration and controlled pilot.
12. Production and push milestones only after pilot evidence.

This ordering assumes the synthesized `.git` decision. If M0.5 selects a real
partial clone, move the minimum upload-pack/promisor portion of item 10 ahead of
item 5; the rest of M5 may remain later.

## 14. Definition of done

The project is not done merely when a filesystem mounts. A production release is
done when:

- a branch selector is pinned and all APIs are snapshot-consistent;
- a mounted commit stays readable for the life of the job regardless of upstream ref
  changes or garbage collection, using an atomically issued and heartbeat-renewed
  lease whose internal ref is neither advertised nor pruned;
- supported POSIX and Git behaviors have conformance tests, including the documented
  divergences: raw blob bytes, sanitized stable base timestamps, and the exact
  `.git`/shim surface;
- search matches documented semantics, includes overlay changes, reports execution
  status separately from scoped coverage, and never presents truncation, missing
  terminal status, backend failure, or a policy/index exclusion as an exhaustive
  empty answer;
- server search produces zero client hydration;
- overlay data survives tested crashes and exports reproducibly;
- **repository** authorization is uniform across Git, file, blob, and search
  paths, and **object** authorization — the mount capability for an unreachable
  pinned commit — holds on the snapshot, blob, and search APIs. It is not
  claimed for the Git gateway, where one bare repository is one authorization
  domain ([ADR 0002](adr/0002-git-object-authorization-boundary.md));
- caches are verified, bounded, isolated, and collectible;
- hosted FUSE privilege and lifecycle are operationally safe;
- real agent tasks preserve correctness while reducing measured clone cost;
- upgrades, rollback, disaster recovery, monitoring, and on-call ownership exist.
