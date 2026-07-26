# M1 — Repository and Snapshot API

## Summary

M0 is complete with a conditional go. Nothing exists yet outside `spikes/`, which is
deliberately throwaway measurement code. M1 builds the first production workspace:
the `xvfs-*` Cargo workspace, a repository catalog with crash-consistent mount
retention leases, the libgit2-backed Git object service, the snapshot/blob APIs, and
uniform repository plus object authorization over them.

M1's exit gate (PLAN.md section 4) is what the work is measured against:

- resolve a revision, page a million-entry snapshot one directory at a time, and
  fetch one file without cloning;
- concurrent ref movement cannot produce mixed-commit responses;
- a leased commit survives a force push, a branch deletion, and a full `git gc`; an
  expired lease stops protecting it; resolution and lease creation have no race
  window; a renewed live lease outlives its original TTL;
- lease refs are absent from Git advertisements and survive upstream fetch/prune;
- unauthorized callers cannot infer blob existence through status, timing, cache, or
  error differences.

Two M0 decisions dominate the implementation and are not revisited here: one bare
repository is one authorization domain for the Git path (ADR 0002), and the lease
policy constants are already fixed (ADR 0006 — 30 min TTL, 5 min heartbeat, 15 min
grace, 24 h prune delay, 24 h maximum age, alert at 2 failures).

## Plan

Seven phases, one commit each, each verified with `cargo build`, `cargo test`, and
`cargo clippy -- -D warnings` before committing.

### Phase 1 — M1.1a: workspace and `xvfs-types`

- Root Cargo workspace with the nine crates PLAN.md lists. `xvfs-search`,
  `xvfs-overlay`, and `xvfs-fuse` land as declared placeholders owned by M4/M3/M2.
- `xvfs-types`: `HashAlgorithm`, `ObjectId`, `BytePath`, `EntryKind` re-homed from
  `spikes/git-probe/src/model.rs`, plus revision selectors, repository IDs, mount
  IDs, structured error codes, and limits.
- Tooling: `xtask`-style scripts for fmt, clippy, test, `cargo-deny`, SBOM, and
  secret scanning. The license check asserts ADR 0001's dependency table directly,
  including the statically linked GPL-2.0 libgit2 that crate metadata misses.

### Phase 2 — M1.1b: `xvfs-proto`

- Protobuf definitions for the snapshot, mount, and search services from DESIGN.md
  section 7.3. Paths are `bytes`, object IDs are qualified strings.
- `tonic`/`prost` codegen driven by `protoc-bin-vendored` so `protoc` is not a host
  prerequisite and cannot drift between local and CI.

### Phase 3 — M1.3: `xvfs-git` object service

- `GitRepository` trait and libgit2 implementation re-homed from the spike, with the
  bounded handle pool, format gate, byte-safe traversal, and `/`-suffix directory
  pagination keys preserved.
- Added over the spike: bounded decoded-tree cache, batch stat, ref transactions for
  lease anchors, and an async wrapper over a bounded blocking pool.

### Phase 4 — M1.2: repository catalog, lifecycle, and retention leases

- SQLite catalog: repositories, refs, ref-event outbox, and leases.
- Create/import/mirror, fetch, verify, quarantine, delete state machines with
  per-repository locking and restart reconciliation.
- Leases as the crash-consistent state machine ADR 0006 fixes: under the repository
  lock, resolve and authorize, persist `PREPARING`, create the `refs/xvfs/mounts/*`
  anchor, persist `ACTIVE`, then return the capability.
- Mirror fetch uses explicit refspecs plus `--prune`, never `--mirror`, so upstream
  pruning cannot reach `refs/xvfs/`.
- Webhook and polling ingestion emitting ref events idempotent on
  `(repository_id, ref_name, old_oid, new_oid)`.

### Phase 5 — M1.4: snapshot and blob APIs

- gRPC: `ResolveRevision`, `GetCommit`, `GetEntry`, `ListDirectory`, `BatchGetEntry`,
  `PrepareSnapshot`, `CreateMount`, `RenewMount`, `ReleaseMount`.
- HTTP: file-by-revision and immutable blob endpoints with ETag, range, deadlines,
  response limits, and backpressure.
- `snapshot_time` cataloged once per commit using ADR 0006's clamp, returned from
  every API that reports a commit.
- Observability: structured error codes, request IDs, tracing, metrics, redaction.

### Phase 6 — M1.5: authentication and authorization

- Pluggable authenticator with a dev static-token verifier and the OIDC seam.
- Uniform repository authorization across gRPC, file, blob, and Git paths.
- HMAC mount capability binding subject, repository, commit, mount ID, and expiry;
  required when a snapshot/blob API reaches a commit unreachable from a visible ref.
- Audit records carrying repository, commit, subject, and job — never content,
  tokens, or unvalidated paths.

### Phase 7 — M1.1c: local development stack and exit-criteria verification

- One-command stack: server, seeded fixtures of several sizes, pinned libgit2 and
  stock Git versions asserted at startup.
- `xvfs-test`: fixture matrix re-homed from the spike, plus the million-entry
  generator.
- Exit-criteria tests: million-entry paging, force-push/branch-delete/`git gc`
  survival, lease expiry and renewal, hidden-ref advertisement and prune survival,
  concurrent ref movement, and the existence-inference checks.

## Decisions

### Placeholder crates for M2–M4 are created now

PLAN.md M1.1 lists nine crates as the deliverable. `xvfs-search`, `xvfs-overlay`, and
`xvfs-fuse` have no M1 content. They are created as declared placeholders rather than
omitted so the workspace shape matches the plan and later milestones have a home;
each states which milestone fills it. The alternative — adding them when first needed
— was rejected because the workspace layout is itself the reviewable artifact of
M1.1.

### `protoc` is vendored, not a host prerequisite

`protoc` is absent from the development host. Requiring it would put a version-drift
risk in exactly the place M1.1 is supposed to remove one. `protoc-bin-vendored`
supplies the compiler through Cargo, matching how ADR 0001 vendors libgit2 and for
the same reason: the build has to be ours.

### The catalog lives in `xvfs-server`, not its own crate

PLAN.md's crate list has no `xvfs-catalog`. The catalog, lease state machine, and
ref-event outbox are server-process concerns and go in `xvfs-server` as modules.
Splitting them out is a refactor to do when a second binary needs them.

### OIDC integration is a seam, not an implementation

M1.5 says "integrate the chosen OIDC/workload identity". No identity provider is
reachable, and no provider has been chosen — ADR 0006 leaves it unrecorded. The
authenticator is therefore a trait with a dev static-token verifier, and the JWT/OIDC
verifier is left as the declared seam. Everything M1.5 actually gates on — uniform
repository authorization, the mount capability, object authorization for unreachable
commits, audit records, and the confused-deputy and traversal tests — is implemented
against the trait and is provider-independent. This is a recorded scope reduction,
not an oversight.

## Details

_To be filled in as phases complete._
