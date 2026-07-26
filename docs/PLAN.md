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

Suggested staffing:

- two Rust systems engineers for the server/client path;
- one search/storage engineer from Milestone 2 onward;
- part-time infrastructure, security, and agent-platform support.

Expected elapsed time is roughly 14–18 weeks for a controlled pilot with three
engineers, followed by another 8–16 weeks of hardening for production. The range
depends heavily on FUSE deployment, push support, search-index scale, and the POSIX
behavior required by real builds.

## 2. Milestones

| Milestone | Outcome | Exit gate |
| --- | --- | --- |
| M0: feasibility | Highest-risk assumptions measured | Go/no-go architecture review |
| M1: repository API | Exact revision/tree/file access | API conformance and auth tests pass |
| M2: read-only mount | Lazy snapshot is usable as files | Representative read/build smoke tests pass |
| M3: writable workspace | Crash-safe overlay and patch export | Mutation model and recovery tests pass |
| M4: agent search | Search does not hydrate base and sees edits | Results match materialized `rg` corpus |
| M5: Git compatibility | Stock Git clone/fetch works | Version/protocol matrix passes |
| M6: hosted pilot | End-to-end jobs run safely | Performance and reliability gates met |
| M7: production | Scaled, operable, supportable service | SLO/security/DR review passes |
| M8: native commit/push | Optional direct Git write workflow | CAS and interoperability tests pass |

M8 can move before M7 if direct pushes are required for the pilot. Otherwise patch
export keeps the critical path smaller.

## 3. M0 — Feasibility and architecture spikes

Duration: 2 weeks

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
- Build the `GitRepository` trait and a libgit2-backed proof of concept.
- Define the blocking-worker and request-local handle model around libgit2.
- Proxy smart HTTP v2 to sandboxed stock `git upload-pack` and run clone/fetch,
  shallow-clone, and partial-clone smoke tests.
- Validate the exact GET advertisement and POST stateless-RPC subprocess contracts.
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

Exit: demonstrate correct results and acceptable projected storage/query cost.

### M0.5 Product decisions

- Answer the open questions in the design.
- Freeze MVP compatibility boundaries and performance gates.
- Define the server/client API versioning policy and repository path semantics.
- Threat-model the proposed host cache and FUSE privilege boundary.
- Create an initial failure-mode and data-retention policy.

Go/no-go gate: proceed only if lazy mount works on the target platform, search index
storage is viable, and projected task savings are meaningful over partial clone.

## 4. M1 — Repository and snapshot API

Duration: 3 weeks

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
  secret scanning, and reproducible release builds.
- Establish structured error codes, request IDs, tracing, metrics, and redaction.

### M1.2 Repository catalog and lifecycle

- Define repository IDs independently from display names and filesystem paths.
- Implement create/import/mirror, fetch, verify, maintenance, quarantine, and delete
  state machines.
- Store upstream configuration and credential references, not raw secrets.
- Implement webhook and polling ingestion with idempotent ref events.
- Add per-repository locking and reconciliation after partial failure.

### M1.3 Git object service

- Implement algorithm-generic OID parsing and canonical verification.
- Resolve branch, tag, full commit OID, and allowed abbreviation safely.
- Read commits, annotated tags, trees, blobs, modes, and object sizes.
- Implement byte-safe tree traversal, directory pagination, and batch stat.
- Cache decoded immutable trees with bounded memory.
- Reject unreachable or unauthorized object access.

### M1.4 Snapshot and blob APIs

- Define and implement `ResolveRevision`, `GetEntry`, `ListDirectory`,
  `BatchGetEntry`, and `PrepareSnapshot`.
- Implement file-by-revision and immutable blob HTTP endpoints.
- Add ETag, range handling, cancellation, deadlines, response limits, and
  backpressure.
- Ensure branch convenience calls return the resolved commit.
- Add API compatibility/golden tests for Protobuf and JSON errors.

### M1.5 Authentication and authorization

- Integrate the chosen OIDC/workload identity.
- Enforce repository permissions uniformly across Git, RPC, file, and search APIs.
- Add short-lived mount credentials or host-daemon delegation.
- Add audit records without logging source content, tokens, or unsafe paths.
- Test confused-deputy, path traversal, unauthorized OID, and stale credential cases.

Exit criteria:

- A test can resolve a revision, list a million-entry snapshot one directory at a
  time, and fetch an individual file without cloning.
- Concurrent ref movement cannot produce mixed-commit responses.
- Unauthorized users cannot infer blob existence through status, timing within a
  defined tolerance, cache, or error differences.

## 5. M2 — Read-only FUSE client

Duration: 3–4 weeks

### M2.1 Mount lifecycle

- Implement `xvfs mount`, `unmount`, `inspect`, and daemon health commands.
- Persist mount identity, repository, pinned commit, API version, and format version.
- Resolve branch once and show the pinned commit in CLI output.
- Handle signals, forced teardown, stale mounts, daemon restart, and job cleanup.
- Implement the chosen host-daemon/CSI integration skeleton.

### M2.2 Metadata and inode model

- Implement stable base inode assignment, inode lookup counts, and forget handling.
- Implement `lookup`, `getattr`, `opendir`, `readdirplus`, `releasedir`, `readlink`,
  `access`, and `statfs`.
- Preserve byte paths and Git modes.
- Define base timestamps and nanosecond behavior.
- Add positive/negative cache TTL and invalidation rules.
- Represent gitlinks and unsupported object modes predictably.

### M2.3 Blob cache and reads

- Implement whole-blob download, temporary-file verification, atomic publication,
  open-handle pinning, and LRU eviction.
- Verify `blob <size>\0<content>` with the repository hash algorithm.
- Implement `open`, `read`, `lseek`, `flush`, `release`, and read-only `mmap`
  behavior as supported by FUSE/kernel.
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
- Compare visible snapshot metadata and file bytes to a materialized Git checkout.

Exit criteria:

- Cold mount meets the startup/download target.
- Reading selected files transfers only required metadata and blobs.
- The chosen representative read-only build or analysis tasks succeed.
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
  configurable policy.
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
- Merge context, ordering, limits, and exit codes predictably.
- Add `--json` and ripgrep-like text output.
- Implement `xvfs-rg` for the selected safe flag subset and fail closed on
  unsupported flags unless explicit hydration is requested.
- Publish agent instructions and an MCP/native tool schema.

### M4.6 Search correctness and performance

- Materialize the same commit and compare XVFS results to `rg` for a large generated
  query corpus.
- Cover CRLF, files without final newline, Unicode, invalid UTF-8 paths, repeated
  blobs, symlinks, binary files, huge lines, regex corner cases, and overlay edits.
- Verify a server search fetches zero blobs into the client cache.
- Benchmark cold/warm branch and arbitrary-commit preparation.

Exit criteria:

- Supported literal/regex results match the documented `rg` semantics.
- Search after edits returns the merged logical workspace.
- Warm search meets the performance target and causes zero base hydration.

## 8. M5 — Git smart HTTP compatibility

Duration: 2–4 weeks

### M5.1 Smart HTTP gateway

- Implement info/refs and upload-pack routes with streaming and backpressure.
- Pass `Git-Protocol` v2 negotiation correctly.
- Disable hooks and unsafe repository/path/environment behavior.
- Add authentication, authorization, request-size, CPU, memory, time, and process
  limits.
- Scrub progress/error output and attach request telemetry.

### M5.2 Protocol feature matrix

- Test protocol v0/v1 fallback as required and v2 `ls-refs`/`fetch`.
- Test thin packs, sideband, shallow fetch, partial-clone filters, ref-in-want, and
  cancellation according to supported scope.
- Test empty, corrupt, alternates-based, SHA-1, and later SHA-256 repositories.
- Test multiple maintained Git client versions on Linux and at least one other OS.
- Fuzz HTTP routing, headers, repository selection, subprocess setup, and response
  streaming at the Rust trust boundary; send malformed pkt-line inputs through the
  sandbox to verify resource limits and safe failure.

### M5.3 Subprocess and repository isolation

- Invoke `git upload-pack` directly without a shell or user-controlled executable
  path, arguments, working directory, environment, or Git configuration.
- Validate and allow-list `Git-Protocol` values before setting `GIT_PROTOCOL`.
- Disable hooks and unsafe configuration and make the repository read-only to the
  upload-pack process.
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
- Install CLI, `xvfs-rg`, agent instructions, and optional MCP tool.
- Collect patch/export as a job artifact and feed it into the existing review/commit
  pipeline.
- Handle cancellation, timeout, node drain, orphan mounts, and cleanup retries.

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

Pilot gate:

- no statistically meaningful correctness regression;
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

- Define reachability roots: refs, active mounts, retained commits, and exports.
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
| Git | client versions, protocol versions, SHA-1/SHA-256, shallow/partial, packed/loose |
| Trees | empty, million files, deep, huge directory, non-UTF-8, mode changes, submodule |
| Files | empty, text, binary, CRLF, huge line, huge blob, symlink, executable |
| Network | latency, loss, retry, timeout, reset, truncated/corrupt response |
| Storage | disk full, read-only, inode full, corruption, eviction, restart |
| Concurrency | duplicate fetch, parallel readers/writers, rename/unlink while open |
| Security | unauthorized repo/OID, path traversal, symlink escape, regex abuse, token expiry |
| Lifecycle | crash, forced unmount, orphan cleanup, upgrade, downgrade, GC race |
| Agent | raw `rg`, XVFS search, edit-then-search, build, test, diff/export |

Correctness oracles:

- stock Git checkout/tree/object output;
- `ripgrep` over a fully materialized pinned checkout;
- an in-memory overlay state model;
- `git apply`/tree comparison for exported patches;
- C Git upload-pack for protocol behavior.

## 13. Initial backlog ordering

The first executable backlog should be:

1. M0.1–M0.4 spikes in parallel where staffing permits.
2. Architecture review and frozen MVP boundary.
3. Workspace/types/protocol foundation.
4. Exact revision and file API.
5. Minimal read-only remote mount.
6. Verified shared blob cache.
7. Overlay state model, then mutations and export.
8. Literal search index and snapshot manifest.
9. Overlay-aware `xvfs search` and agent tool integration.
10. Smart HTTP compatibility matrix.
11. Hosted orchestrator integration and controlled pilot.
12. Production and push milestones only after pilot evidence.

## 14. Definition of done

The project is not done merely when a filesystem mounts. A production release is
done when:

- a branch selector is pinned and all APIs are snapshot-consistent;
- supported POSIX and Git behaviors have conformance tests;
- search matches documented semantics and includes overlay changes;
- server search produces zero client hydration;
- overlay data survives tested crashes and exports reproducibly;
- authorization is uniform across Git, file, blob, and search paths;
- caches are verified, bounded, isolated, and collectible;
- hosted FUSE privilege and lifecycle are operationally safe;
- real agent tasks preserve correctness while reducing measured clone cost;
- upgrades, rollback, disaster recovery, monitoring, and on-call ownership exist.
