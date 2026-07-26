# XVFS: Agent-Oriented Virtual Git Workspace

Status: Draft; core implementation choices accepted  
Audience: engineering, infrastructure, security, and agent-platform teams  
Target platform: Linux-hosted coding agents  

## 1. Summary

XVFS is a Git-compatible repository service and a Rust FUSE client for short-lived,
hosted coding-agent jobs. A job mounts an immutable Git snapshot, sees a normal
directory tree, and downloads file data only when a process opens a file. Writes go
to a local copy-on-write overlay. Code discovery uses a revision-aware server search
API so that searching a monorepo does not hydrate the entire working tree.

The closest existing options solve parts of the problem:

- Git partial clone can omit blobs and fetch missing objects on demand, but dynamic
  object fetching may be one object at a time and ordinary checkout/search behavior
  can still pull a large working set. Git's own design notes call out that cost.
- Sparse checkout and Scalar help when the needed paths are known in advance. An
  agent often needs repository-wide discovery before it knows those paths.
- VFS for Git presents a virtual working directory and downloads objects on demand,
  but its published implementation is Windows-oriented, needs a cooperating Git
  service, and now recommends Scalar for new deployments.
- EdenFS is an important architectural reference, but adopting its wider source
  control stack is a much larger integration than a small Git-compatible service.

There is therefore room for an agent-specific solution. The novel part is not FUSE
alone; it is the combination of a pinned snapshot, copy-on-write edits, remote
revision-correct code search, hydration accounting, and an agent-facing tool
contract.

## 2. Goals

1. Start a workspace without cloning repository history or materializing every file.
2. Present enough POSIX filesystem behavior for editors, compilers, and common agent
   tools.
3. Fetch directory metadata and file blobs lazily and cache immutable data across
   jobs on the same trusted host.
4. Search tracked source at an exact branch tip or commit without reading every file
   through FUSE.
5. Preserve local edits, additions, deletions, renames, executable bits, and
   symlinks in a crash-consistent overlay.
6. Export a patch and, later, create and push ordinary Git commits.
7. Allow unmodified Git clients to clone/fetch from the server using Git smart HTTP
   protocol v2.
8. Make authorization, auditing, observability, and resource limits first-class.

## 3. Non-goals

The first production version will not:

- replace all Git porcelain;
- transparently make every Git command fast over the mounted tree;
- support macOS or Windows filesystems;
- provide a general distributed search engine;
- index generated or untracked build outputs;
- recursively mount submodules;
- provide offline access to files that were never cached;
- guarantee that arbitrary programs cannot hydrate the tree. FUSE sees file opens,
  not the user's intent behind them.

## 4. User experience

An orchestrator creates one mount per job:

```text
xvfs mount \
  --repo acme/monorepo \
  --rev refs/heads/main \
  --state /var/lib/xvfs/jobs/job-123 \
  /work/repo
```

The client resolves `refs/heads/main` once and pins the resulting commit. The mount
does not change under the process if the branch advances.

The agent uses normal file operations for known files and a dedicated command for
discovery:

```text
xvfs search 'RequestContext' --glob 'services/**/*.rs'
xvfs search --regex 'fn\s+authorize_' --json
xvfs status
xvfs diff --format git
```

`xvfs search` combines server results for the pinned base commit with a local search
of modified and newly created overlay files. Deleted or replaced base paths are
removed from server results. This is necessary for search to remain correct after
the agent starts editing. A result set that was cut short by a budget, or that could
not cover part of the workspace, says so; it never looks like an empty result.

Familiar commands keep working where it is cheap to make them work. The mount
carries a synthesized read-only `.git`, and a `git` shim answers `status`, `diff`,
`rev-parse`, `ls-files`, `show`, and pinned-commit `log` from the overlay journal and
the snapshot API. Section 8.6 covers the boundary and what happens outside it.

At job completion the orchestrator runs `xvfs diff` or `xvfs export`. A later phase
adds `xvfs commit` and compare-and-swap branch updates.

## 5. System context

```mermaid
flowchart LR
    G[Standard Git client] -->|smart HTTP v2| GS[Git compatibility gateway]
    A[Agent process] -->|POSIX I/O| F[FUSE workspace daemon]
    A -->|xvfs search/status/diff| C[XVFS CLI]
    F --> MC[Metadata cache]
    F --> BC[Host blob cache]
    F --> O[Per-job writable overlay]
    C --> O
    F -->|resolve/tree/blob API| API[Snapshot API]
    C -->|search API| S[Search service]
    GS --> R[Authoritative bare repo or mirror]
    API --> R
    S --> I[Blob index + snapshot manifests]
    S --> R
    W[Ref watcher/index workers] --> R
    W --> I
```

The Git object database remains authoritative. Search indexes, decoded-blob caches,
manifests, and client caches are derived and can be rebuilt.

### 5.1 Accepted implementation choices

The prototype will use the following implementation:

- the server and client applications are written in Rust;
- server-side repository access uses libgit2 through the `git2-rs` bindings;
- stock `git upload-pack` is executed as a sandboxed child process behind the Rust
  smart-HTTP gateway for clone/fetch compatibility;
- the Linux filesystem uses `cberner/fuser`;
- the FUSE client reads individual files through the XVFS snapshot/blob API, not
  through `upload-pack`;
- the mount presents a synthesized read-only `.git` plus a `git` shim, with a real
  partial-clone `.git` as a measured alternative (section 8.6).

Using libgit2 and `upload-pack` means the deployable server includes C components,
but application control flow, APIs, authorization, search, orchestration, and
filesystem behavior remain implemented in Rust. A native Rust Git wire-protocol
engine is not in the planned scope.

This choice constrains the repositories XVFS can host. libgit2 cannot read the
`reftable` ref backend that stock Git 2.45 and later can create, and its SHA-256
support is experimental and requires a non-default build. Server-side bare
repositories are therefore created and maintained with the `files` ref backend, and
the repository catalog rejects a mirror whose format libgit2 cannot open rather than
serving a partial view of it. Section 12 records the consequence for SHA-256.

## 6. Core design decisions

### 6.1 Git is the source of truth

XVFS does not invent a new commit, tree, or blob format. Repository data remains
valid Git data, addressed by the repository's object hash algorithm. Internal IDs
must carry the algorithm as well as the digest; code must not assume a 20-byte
SHA-1 object ID because Git pack formats also support SHA-256 repositories.

Use libgit2 through `git2-rs` for object parsing, tree traversal, refs, diffs, object
creation, revision walks, and object-database access. Isolate it behind a
`GitRepository` trait so FFI lifetimes, blocking work, library upgrades, and
conformance details do not leak into HTTP, search, or FUSE code. Repository and
pack-builder handles are not shared concurrently; blocking libgit2 operations run
in bounded worker pools.

The trait boundary also exists because libgit2 does not cover every repository
format stock Git can produce. Supported repository formats are an explicit,
validated property of a mirror, not an assumption.

### 6.2 Pin every workspace to an immutable commit

A branch name is only a selector. `CreateMount` or `ResolveRevision` returns a
commit OID, and all later tree, blob, and search requests name that OID. Responses
repeat the resolved commit. This prevents a file from one branch generation being
combined with search results or metadata from another generation.

An explicit `xvfs refresh` may move a clean workspace to a new commit, but it does
so by creating a new mount generation and replacing the old bind mount rather than
mutating a live FUSE namespace in place. This preserves the immutable-base
assumption, avoids stale kernel dentries under long TTLs, and gives open handles
well-defined old-generation behavior. Refresh with local changes requires a
three-way rebase operation and is not part of the MVP.

### 6.3 Keep extensions outside the Git wire protocol initially

Git protocol v2 is command-oriented and extensible, but file, tree, and search APIs
have different caching, streaming, error, and authorization needs from pack
negotiation. The first version exposes:

- standard Git smart HTTP endpoints for stock Git clients;
- a versioned gRPC snapshot/search API for the Rust client;
- an HTTP immutable-blob endpoint for range requests, CDN caching, and debugging.

A future server can advertise custom protocol-v2 commands, but they should be a
gateway onto the same internal services rather than a second implementation.

### 6.4 Use a writable overlay, not a writable remote snapshot

The mounted base is immutable. Local mutations are recorded in a per-job overlay:

- a journal/catalog records path state and metadata;
- created or modified content is stored as regular local files;
- a whiteout represents a deleted base path;
- a rename is journaled atomically;
- the executable bit and symlink target are preserved.

On the first write to an existing file the client copies the blob into the overlay.
`O_TRUNC` avoids downloading old content when the caller is replacing the whole
file. Reads use overlay content first and fall back to the pinned base.

This model makes failure recovery and patch export tractable. It also avoids a
virtual `.git/index` whose behavior would be tightly coupled to every Git command.

### 6.5 Search immutable blobs, filter by snapshot membership

Indexing every path/content occurrence for every commit would multiply storage.
Instead, each unique searchable blob in a repository receives a stable numeric
`blob_key` and is indexed once. A snapshot manifest supplies:

- a sorted `path -> (mode, object_id, blob_key)` table;
- a `blob_key -> path(s)` reverse table;
- a Roaring bitmap containing blob keys reachable from the snapshot.

Literal search uses a trigram posting index whose values are blob-key bitmaps.
Posting lists are intersected with the snapshot bitmap, then candidate bytes are
verified to produce exact line, column, and snippet results. Regex search extracts
required literals/trigrams when possible and uses `regex-automata` for bounded
verification. Broad regexes with no useful literal are subject to scan and time
budgets.

Tantivy provides optional tokenized/ranked full-text search. Each indexed document
stores `blob_key` as a fast field. A custom snapshot filter maps the stable
blob-key bitmap into segment-local document bitmaps and caches that mapping.
Literal/regex search is the default for code because its behavior is closer to
`ripgrep`; token search is an explicit mode.

Configured branch tips are prepared eagerly. For an arbitrary commit, the server
builds and caches the immutable snapshot manifest on demand. The API may return an
operation ID when preparation exceeds the request deadline. It never silently
searches an older branch tip.

## 7. Server architecture

### 7.1 Repository manager

The repository manager owns authoritative bare repositories or read-through mirrors
of an upstream Git host.

Responsibilities:

- create, fetch, repack, verify, and remove mirrors;
- validate that a mirror's repository format is supported before serving it;
- resolve refs, tags, and abbreviated object IDs under an authorization context;
- serialize ref mutations per repository;
- emit a durable ref-change event after a successful update;
- reconcile catalog state with refs after a crash or missed webhook;
- keep commits pinned by live mounts reachable;
- enforce repository quotas and run Git maintenance.

The retention rule is not deferrable. A mounted commit can become unreachable at any
time — a force push, a deleted branch, or an upstream rebase — and the next
`git gc` would then prune objects out from under a running job, turning every
uncached read into a permanent error mid-task.

Mount creation is therefore one server-side operation under the repository's
ref/maintenance lock: resolve the selector, authorize the resolved commit, write a
durable `PREPARING` lease record, create its reachability anchor, mark the lease
`ACTIVE`, and only then return the mount capability. The catalog and Git ref cannot
share one storage transaction, so restart reconciliation removes abandoned
`PREPARING` anchors or completes a provably safe transition. An `ACTIVE` lease is
never returned before its anchor is durable. There is no client-visible gap between
revision resolution and lease activation.

The anchor is a ref under a reserved namespace such as
`refs/xvfs/mounts/{mount_id}`. The namespace is hidden from upload-pack and ordinary
ref enumeration, excluded from upstream mirror and prune refspecs, and rejected as a
user-supplied revision. It is an implementation reachability root, not a published
repository ref or an authorization mechanism. Maintenance and garbage collection
treat live leases as roots.

Leases have a bounded TTL and must be renewed by an authenticated daemon heartbeat.
Renewal is idempotent, extends the catalog expiry, and verifies or repairs the
durable anchor while holding the repository lock; the anchor itself has no TTL.
Unmount and job cleanup release the lease eagerly; expiry is the crash fallback.
The server keeps a grace interval between expiry and pruning so a transient renewal
failure can be reported and recovered without immediately destroying a live
workspace.

Every mount receives an unforgeable capability binding subject, repository, commit,
mount ID, and expiry. Snapshot calls for a commit that is no longer reachable from a
public ref must present that capability; repository access alone does not grant
access to every commit retained for another mount. Section 7.6 and the production
milestone extend the same lease mechanism to derived data.

For a proxy deployment, ingestion uses webhooks plus polling. For an authoritative
deployment, receive-pack creates the ref-change event. Ref events are idempotent and
keyed by `(repository_id, ref_name, old_oid, new_oid)`.

### 7.2 Git compatibility gateway

Required initial compatibility is read-only Git smart HTTP:

- `GET .../info/refs?service=git-upload-pack`;
- `POST .../git-upload-pack`;
- protocol v2 `ls-refs` and `fetch`;
- shallow clone and an explicitly configured partial-clone filter subset.

Push/`git-receive-pack`, SSH transport, and unauthenticated `git://` are later
milestones. Smart HTTP is the best first transport for hosted jobs because it works
with existing TLS, proxies, and bearer credentials.

The accepted implementation is a Rust gateway that authenticates, authorizes,
limits, and streams requests to a sandboxed stock `git upload-pack` child process.
No shell is involved; the executable, arguments, repository path, accepted
`GIT_PROTOCOL` values, configuration, environment, and resource limits are fixed by
the gateway.

`upload-pack` is Git's read-side wire service: it advertises refs and capabilities,
accepts the client's wanted and existing objects, calculates the missing object
closure, invokes pack generation, and returns a packfile. It does not handle pushes;
that is the separate `receive-pack` service.

For smart HTTP, the gateway maps the two request phases as follows:

1. `GET .../info/refs?service=git-upload-pack` runs
   `git upload-pack --http-backend-info-refs <bare-repository>` and returns the
   capability/ref advertisement as `application/x-git-upload-pack-advertisement`
   with `Cache-Control: no-cache`. The subprocess emits only the protocol
   advertisement body. For protocol v0/v1, the gateway first writes the
   `# service=git-upload-pack` pkt-line and a flush packet, exactly as
   `git-http-backend` does. For protocol v2 it must not write that preamble; the
   response begins directly with the subprocess's `version 2` pkt-line.
2. `POST .../git-upload-pack` runs
   `git upload-pack --stateless-rpc <bare-repository>`, streams the request to
   stdin, and streams stdout as `application/x-git-upload-pack-result`. Git clients
   may send the request body with `Content-Encoding: gzip`; the gateway decompresses
   it under an explicit output-size and ratio bound before forwarding.

For protocol v2, the gateway validates the `Git-Protocol` header and passes the
negotiated value in `GIT_PROTOCOL`. The subprocess implements pkt-line framing,
`ls-refs`, want/have negotiation, shallow behavior, partial-clone filters, pack
deltas, and sideband output. XVFS still owns authentication, repository selection,
HTTP validation, deadlines, cancellation, concurrency limits, and auditing.

Partial-clone support is controlled configuration, not merely a property of the Git
binary. The gateway enables `uploadpack.allowFilter` only when repository policy
allows it, disables unselected filter families with
`uploadpackfilter.<filter>.allow`, and validates the exact requested filter before
starting the subprocess. That gateway validation matters because Git's per-filter
configuration can allow a family more broadly than XVFS intends. The initial target
is exactly `blob:none`; `blob:limit` and bounded `tree:<depth>` are enabled only if
M0 measurements require them. Broad or future filter forms are denied by default.
Unadvertised-object features such as `allowAnySHA1InWant` remain disabled. The same
protected configuration hides `refs/xvfs/` from advertisement and fetch.

libgit2's pack builder is useful for XVFS-created objects and later commit/export
features, but libgit2 does not provide a drop-in server-side upload-pack state
machine. Reimplementing capability advertisement, negotiation, filtering,
reachability security, and sideband streaming on libgit2 would add substantial
compatibility risk without improving the agent workspace. It is therefore not part
of the planned implementation.

The snapshot and blob services never invoke `upload-pack`. They use `git2-rs`
directly to resolve a pinned commit, traverse its tree, find the blob OID, and return
decoded file bytes. This avoids pack negotiation and construction for a one-file
FUSE read.

### 7.3 Snapshot service

Proposed RPCs:

```text
ResolveRevision(repo, revision_selector)
  -> { commit_oid, tree_oid, ref_name?, ref_version, snapshot_time }

CreateMount(repo, revision_selector, requested_ttl)
  -> { mount_id, commit_oid, tree_oid, ref_name?, snapshot_time,
       mount_capability, lease_expiry }

RenewMount(mount_id, mount_capability)
  -> { mount_capability, lease_expiry }

ReleaseMount(mount_id, mount_capability)
  -> {}

GetCommit(repo, commit_oid, snapshot_authorization?)
  -> { commit_oid, tree_oid, parent_oids[], author, committer, message,
       snapshot_time }

GetEntry(repo, commit_oid, path_bytes, snapshot_authorization?)
  -> { kind, mode, oid, size, symlink_target?, blob_ticket? }

ListDirectory(repo, commit_oid, path_bytes, page_token, page_size,
              snapshot_authorization?)
  -> { entries[], next_page_token }

BatchGetEntry(repo, commit_oid, path_bytes[], snapshot_authorization?)
  -> { results[] }

PrepareSnapshot(repo, commit_oid, snapshot_authorization?)
  -> { state: READY | BUILDING | FAILED, operation_id?, failure_reason? }

Search(repo, commit_oid, query, mode, path_globs, limits,
       snapshot_authorization?)
  -> stream {
       match { path_bytes, line, column, snippet, blob_oid, commit_oid }
       | completion {
           execution_status: COMPLETE | TRUNCATED,
           truncation?,
           coverage {
             eligible_paths,
             excluded_paths_by_reason[],
             requested_scope
           },
           index_generation
         }
     }
```

Paths are bytes in the protocol, not assumed UTF-8. CLI JSON includes a lossless
escaped representation.

`snapshot_authorization` is either normal proof that the commit is reachable from a
currently allowed ref or the mount capability returned by `CreateMount`. This is
what lets one subject keep using its force-pushed-away pinned commit without making
that retained commit available to every repository reader.

`blob_ticket` is the short-lived blob authorization described below.
`PrepareSnapshot` reports the three lifecycle states used everywhere in the system;
`NOT_INDEXABLE` and `RESOURCE_LIMIT` are request error codes, not snapshot states.
A successful `Search` RPC ends with exactly one completion message. EOF, transport
reset, cancellation, or RPC error before that terminal message is a failed search,
never a complete partial result.

The convenience file endpoint is:

```text
GET /v1/repos/{opaque_repo_id}/file?rev={selector}&path_b64url={encoded_path}
```

It atomically resolves the selector and returns headers containing the resolved
commit, Git mode, blob OID, and size. The low-level immutable endpoint is:

```text
GET /v1/repos/{opaque_repo_id}/blobs/{algorithm}:{oid}?ticket={short_lived_ticket}
Range: bytes=start-end
If-None-Match: "{algorithm}:{oid}"
```

Using base64url for the byte path avoids ambiguous URL normalization of non-UTF-8
names and encoded slashes. Blob authorization requires both repository access and
proof that the requested blob is reachable from an allowed revision. `GetEntry` or
the convenience endpoint returns a short-lived ticket binding repository, commit,
blob OID, subject, and expiry; a CDN/object-store signed URL can carry the same
decision.

### 7.4 Tree and blob serving

Git trees are already hierarchical manifests, so listing one directory only needs
the commit/tree path plus the immediate tree object. `GetEntry` traverses tree
objects along one path. Server-side caches store decoded trees by OID.

For the MVP, opening a file downloads the complete decoded blob. Whole-blob caching
is simple, makes Git OID verification straightforward, and is appropriate for most
source files. A later large-file extension may return a chunk manifest and serve
byte ranges. That extension must still verify the reconstructed Git blob hash and
must not confuse chunk hashes with Git object IDs.

Blob responses are immutable, use OID-based ETags, and may be cached on local SSD or
object storage. The server verifies the Git object header and digest before
publishing a derived decoded blob.

### 7.5 Search ingestion

The index pipeline for a ref update is:

1. Resolve and validate the new commit.
2. Diff its root tree against the previous indexed tip when possible.
3. Assign blob keys and index unseen textual blobs.
4. Build the new path table, reverse path table, and reachability bitmap.
5. Commit index segments and the snapshot manifest.
6. Atomically mark `(repo, commit)` ready.

A first snapshot or unrelated commit is built by a parallel tree walk. Repeated
blobs are deduplicated by OID. Binary files and blobs over a configurable limit
(suggested default: 8 MiB) are excluded from content search but remain available
through file APIs. Index state records the exclusion reason. Generated and vendored
tracked text is classified for observability and optional policy, but is not
excluded by default because `xvfs-rg` would otherwise silently diverge from ordinary
`rg`.

Search responses carry the exact commit OID and index generation. Time, result,
candidate-byte, and regex-complexity limits are enforced server-side.

Silence must never be ambiguous. An agent that receives an empty or short result set
will conclude the symbol does not exist, so execution status and corpus coverage are
separate parts of the response contract:

- **Execution status** says whether the query finished evaluating the declared
  searchable corpus. Result, time, candidate-byte, regex, cancellation, and backend
  limits produce `TRUNCATED` or an RPC failure and a non-success CLI exit.
- **Coverage** describes files outside that corpus, grouped by reason, within the
  requested path-glob scope rather than across the entire repository. A normal
  indexed query can be execution-complete while reporting binary or oversized
  exclusions.

The CLI renders both dimensions on stderr in text mode and as explicit fields in
`--json`. Policy coverage gaps are warnings under the normal mode; a
`--require-exhaustive` mode treats any relevant coverage gap as failure and may use
a separately budgeted server-side fallback scan where policy permits. A missing
terminal completion message is always an error. This makes a budget-truncated query
impossible to mistake for absence without making every search in a repository that
contains one binary file permanently fail.

### 7.6 Storage

Single-node prototype:

- bare Git repositories on local SSD;
- SQLite for repository, ref, job, and index catalogs;
- immutable manifest/index files on local SSD;
- decoded blob cache on local SSD;
- Tantivy plus custom trigram and Roaring-bitmap files.

Scaled deployment:

- shard repositories to owner nodes instead of distributing one search query;
- PostgreSQL for the control-plane catalog and durable job/outbox state;
- object storage for immutable decoded blobs, manifests, and index snapshots;
- local NVMe caches for packs and active search indexes;
- replicas load immutable index generations and swap them atomically;
- a queue delivers idempotent indexing and garbage-collection work.

Tantivy is a library, not a distributed service. Repository sharding keeps the
system understandable and allows an index to be rebuilt from Git if derived storage
is lost.

### 7.7 Service technology

Accepted implementation stack:

| Concern | Technology | Rationale |
| --- | --- | --- |
| Runtime | Tokio | Mature async networking and cancellation |
| HTTP/Git gateway | Axum, Hyper, Tower | Streaming bodies and middleware composition |
| RPC | Tonic + Prost | Typed streaming API shared by server and client |
| TLS | rustls | Rust-native TLS |
| Git objects | libgit2 via `git2-rs` | Mature repository, ODB, tree, diff, ref, and pack-building APIs |
| Git wire read service | stock `git upload-pack` child process | Exact clone/fetch protocol behavior without reimplementing negotiation |
| FTS | Tantivy | Embedded Rust token search and immutable segments |
| Literal/regex | custom trigrams + `regex-automata` | Exact code-search semantics with bounded matching |
| Snapshot sets | `roaring` | Compact and fast bitmap intersections |
| Prototype catalog | SQLite via `rusqlite` | Simple transactions and recovery |
| Production catalog | PostgreSQL via SQLx | Durable multi-node coordination |
| Serialization | Protobuf for APIs; versioned internal formats | Explicit compatibility |
| Telemetry | `tracing`, OpenTelemetry, Prometheus metrics | Cross-client request and hydration traces |
| CLI/config/errors | Clap, Serde, `thiserror`/`anyhow` | Conventional Rust tooling |

Exact versions should be pinned after the spikes in the plan, followed by automated
dependency update and compatibility testing.

## 8. Client architecture

### 8.1 Processes and deployment

`xvfs` is the user CLI. `xvfsd` owns the FUSE session, caches, network clients, and
overlay.

FUSE inside a container generally requires `/dev/fuse` and additional privilege.
The preferred hosted deployment runs `xvfsd` as a trusted host service or Kubernetes
CSI node component, then bind-mounts a per-job workspace into the unprivileged agent
container. Direct in-container mounting is supported only on platforms that safely
expose FUSE. A selective `xvfs materialize` mode is a fallback for environments
where mounting is impossible, but is not the primary architecture.

Credentials belong to the host daemon where possible. A job receives only a mount
handle and scoped CLI socket, reducing token exposure inside the workload.

### 8.2 FUSE operation mapping

`xvfsd` implements the `fuser::Filesystem` trait from `cberner/fuser`. Network and
other blocking storage work never runs on a FUSE callback thread without a bound;
callbacks submit work to bounded executors and preserve cancellation and request
deadlines.

| Operation | Behavior |
| --- | --- |
| `lookup`, `getattr` | Overlay lookup, then cached/remote entry lookup |
| `readdirplus` | Merge base directory page with overlay additions, renames, and whiteouts |
| `open`, `read` | Overlay file or immutable blob cache; fetch once on miss |
| `create`, `mkdir` | Allocate overlay inode and journal transaction |
| `write`, `truncate` | Copy up when needed, then mutate local overlay |
| `unlink`, `rmdir` | Remove overlay entry or create base whiteout |
| `rename` | Atomic overlay journal transaction; copy-up metadata as needed |
| `readlink`, `symlink` | Preserve Git symlink semantics with safety policy |
| `setattr` | Support size, executable bit, and timestamps subject to the overlay time floor; reject unsupported modes |
| `flush`, `fsync`, `fsyncdir` | No-op on base entries; durable through the overlay journal for local files |
| `statfs` | Report overlay quota and free space, not the host filesystem totals |
| `link` | `EPERM` in the MVP; Git has no hard links and the overlay does not model them |
| `fallocate`, `copy_file_range` | Optional overlay passthrough; fall back to a documented error |
| `mmap` | `MAP_PRIVATE` and read-only `MAP_SHARED` supported; writable `MAP_SHARED` depends on the FUSE writeback cache and is a measured decision, not an assumption |
| xattrs, device nodes | Return a documented unsupported error in the MVP |

Base inodes are stable for the life of the mount and derived from the mount identity
plus entry identity. Overlay inodes are allocated persistently. Kernel attribute and
entry TTLs are long because the base commit is immutable; overlay mutations issue
the necessary invalidations.

Timestamps and inode numbers are load-bearing for build systems, so they are
specified rather than incidental. A raw Git committer timestamp cannot be used
directly because Git accepts future-dated commits and job hosts can have clock skew.
When the server first catalogs a commit it durably records a stable sanitized time:

```text
snapshot_time = clamp(committer_time, minimum_supported_time,
                      authoritative_first_seen_time - one_tick)
```

The catalog writer supplies the authoritative first-seen time, and replicas reuse
the stored value rather than recomputing it from their clocks. Every base entry
reports `snapshot_time` for `mtime` and `ctime`.

Overlay timestamp assignment uses a logical clock:

```text
new_time = max(host_wall_clock, snapshot_time + one_tick, prior_entry_time + one_tick)
```

This keeps base times stable across remounts and hosts while guaranteeing that an
acknowledged local mutation is newer even for a future-dated commit or a skewed host
clock. `one_tick` is the timestamp resolution reported by the mount. An explicit
overlay timestamp request is clamped to at least `snapshot_time + one_tick`, and
`ctime` still advances on the logical clock; exact restoration of an older mtime is
a documented MVP incompatibility. Base inode numbers are stable within one mount
generation but not across mounts; tooling that caches build state keyed on
`(device, inode)` between jobs will therefore miss rather than produce a stale hit,
which is the safe direction. Both properties are stated in the compatibility
boundary because a workload that requires the opposite needs a different design.

Git modes supported initially are regular non-executable, regular executable,
symlink, directory, and gitlink. A gitlink appears as an empty, read-only directory
with inspectable XVFS metadata; recursive submodule mounting is explicit and later.

### 8.3 Local state

Each job has:

```text
state/
  mount.json              pinned repo, commit, snapshot time, lease, format version
  overlay.sqlite          path/inode journal and transactions
  files/                  created and copy-up file data
  locks/
  telemetry/
```

The host has an optional cache:

```text
cache/
  trees/
  blobs/
  metadata.sqlite
```

Cache entries are written to a temporary file, hashed as a canonical Git blob, then
atomically renamed. Eviction is quota-based LRU and never removes open entries.
Plaintext deduplication is scoped to a repository or explicitly trusted
authorization domain; cross-tenant global deduplication risks data leakage and
content-existence side channels.

### 8.4 Hydration policy

The client records, per process where available:

- blob count and bytes fetched;
- metadata requests and directory entries fetched;
- cache hits;
- reads avoided by remote search;
- top hydrating commands and paths.

Mount options set soft and hard byte/file budgets. A soft limit emits warnings and
telemetry. A hard limit returns `EDQUOT` for new remote hydration while preserving
overlay and cached access. Hard limits are opt-in because a build may legitimately
need many files.

An `xvfs-rg` compatibility wrapper translates a deliberately small, documented
subset of common `rg` flags into `xvfs search`; unsupported options fail with a
message unless `--hydrate` is explicitly requested. Hosted agent images place this
wrapper early in `PATH` and include an `AGENTS.md` instruction. An MCP or native
agent tool should expose the same search API. This is defense in depth, not a
filesystem guarantee: a program that directly opens every file will still hydrate
every file unless a hard budget stops it.

### 8.5 Status, diff, export, commit

`status` is derived from the overlay journal and does not scan the base tree.
`diff` reads only changed overlay files and their base blobs. It can emit:

- human-readable unified diff;
- Git-compatible patch;
- JSON change manifest plus content files.

Binary and mode changes are represented explicitly. Export is atomic and includes
the base commit so a downstream applier can reject or three-way merge stale work.

Later, `commit` hashes overlay files, rewrites only affected Git trees, creates a
commit, and asks the server to update a branch with:

```text
expected_old_oid = workspace base or last refreshed tip
new_oid = created commit
```

The server performs a compare-and-swap ref transaction. A mismatch never silently
overwrites another writer.

### 8.6 The `.git` directory and Git commands inside the mount

A mounted workspace that contains no `.git` directory is not a usable agent
workspace. Coding agents run `git status`, `git diff`, and `git log` as a matter of
habit, and a large amount of ordinary tooling — build scripts, version stamping,
linters, formatters, language servers, and workspace-root detection in Cargo, Bazel,
and Node tooling — probes for a repository root before doing anything else. If
nothing answers, tools fail in confusing ways and an agent's most likely repair is
`git init`, which produces a second, wrong source of truth inside the job.

The MVP therefore presents a synthesized, read-only `.git` directory at the mount
root containing `HEAD`, a `packed-refs` entry for the pinned revision, a minimal
`config`, and an `xvfs.json` describing the repository, the pinned commit, and the
API endpoint. It contains no object database and no index. This is deliberate about
what it does and does not do:

- repository-root discovery, `git rev-parse --show-toplevel`, `git rev-parse HEAD`,
  and branch-name detection work, which is what most non-Git tooling actually needs;
- commands that require object content fail immediately and visibly rather than
  returning a wrong answer, because a synthesized `.git` with an empty index would
  otherwise report every tracked file as deleted;
- the directory is outside the overlay's change tracking. It is not searchable, does
  not appear in `xvfs status`, and cannot be committed or exported.

Alongside it, a `git` front-end shim — the same pattern as `xvfs-rg`, installed
early in `PATH` in hosted agent images — serves the high-frequency read-only
subcommands from the overlay journal and the snapshot API: `status`, `diff`,
`rev-parse`, `ls-files`, `show HEAD:<path>`, and `log -1` for the pinned commit. The
M0 command inventory freezes an explicit grammar of supported global options,
subcommands, flags, pathspecs, output formats, and exit codes; the shim does not
claim compatibility for an entire subcommand name. `GetCommit` supplies the one
commit of metadata needed by the bounded `log -1` implementation.

For the supported grammar, the shim is both cheaper and more accurate than real Git
would be, because `xvfs status` is derived from the journal and touches no base
metadata. Every other form fails with an actionable message. Under the synthesized
`.git` option there is no generic `--hydrate` escape hatch: no real object database
or index exists to delegate to. If M0 selects the real partial-clone option, an
explicit hydration flag may delegate to stock Git under the mount's hydration
budget. As with `xvfs-rg`, the shim is a usability and hydration-control measure,
not a security boundary; tools that bypass `PATH` are expected to see the
documented limitations of the synthesized `.git`.

The full-fidelity alternative is a real Git repository: a shallow, blobless clone
whose promisor remote is the XVFS smart-HTTP gateway, with the mount serving as its
working tree. It would make substantially more of Git work, but integration is not
assumed to be a small step: index population, checkout suppression, promisor fetch,
working-tree configuration, and reconciliation with the overlay journal all need
testing. Its costs also land exactly where XVFS is trying to save: `git status`
stats every index entry, so each invocation sweeps the metadata of the entire
monorepo; the index for a million-file repository is itself on the order of a
hundred megabytes per job; the required current-tree metadata can exceed the
cold-start download budget; and Git's index becomes a second view of "what changed"
that can disagree with the overlay journal. Commands such as `git checkout` and
`git reset --hard` would also trigger bulk hydration and copy-up.

The decision between the two is a measurement, not a preference, so the plan spikes
it in M0. If the partial-clone option wins, the minimum smart-HTTP/promisor work in
M5 becomes a predecessor of M2 rather than a parallel later milestone.

Two operational details apply to either choice. The mount must be owned by the job's
UID, or Git's `safe.directory` must be configured for it, or Git refuses to operate
in a bind-mounted workspace owned by the host daemon. And whatever occupies `.git`
must be excluded from search, diff, export, and hydration accounting.

### 8.7 Bounding local overlay search

`xvfs search` merges server results for the pinned base with a local search of the
overlay. The server side is bounded by index policy, but the local side is not
bounded by anything unless it is designed to be. A job that has run a build may hold
gigabytes of `target/`, `node_modules/`, or generated output in its overlay, and a
naive scan of every overlay file would make `xvfs search` slower than the `rg`
invocation it replaces — the opposite of the product's purpose.

Local search therefore applies the same class of policy the server index applies:
it honors the `.gitignore` files present in the merged workspace, plus
`.git/info/exclude` when the selected `.git` surface provides one, skips binary and
oversized files using the server's classification rules and limits, and enforces its
own time and bytes-read budget. Anything skipped is reported through
the same two-dimensional completion contract described in section 7.5: exhausted
local budgets affect execution status, while ignored, binary, and oversized paths
affect coverage within the requested scope. An explicit flag searches ignored
files, and pays for it.

## 9. Consistency and failure behavior

- A mount generation's base never changes. Explicit refresh replaces it with a new
  generation rather than modifying it in place.
- Immutable tree/blob caches are keyed by repository hash algorithm and OID.
- Ref resolution has a short TTL; commit/tree/blob data may be cached indefinitely
  subject to space limits.
- A pinned commit stays readable for the life of its mount. An atomically created,
  heartbeat-renewed retention lease keeps it reachable even if the branch is
  force-pushed, deleted, or rebased upstream.
- A search is correct only when its snapshot manifest and index generation are
  ready. A snapshot is `READY`, `BUILDING`, or `FAILED`; `SNAPSHOT_BUILDING`,
  `NOT_INDEXABLE`, and `RESOURCE_LIMIT` are distinct request errors.
- A truncated or failed search is reported as such; corpus exclusions are reported
  separately as scoped coverage. Missing terminal status is an error. No budget,
  index exclusion, or partial failure is allowed to look like an exhaustive empty
  result.
- Network loss returns retryable I/O errors for uncached base data. Overlay writes
  and cached reads continue.
- Overlay mutations commit metadata and file rename/fsync in a documented order.
  On restart, journal recovery either completes or rolls back each transaction.
- Unmount does not discard an overlay. Job cleanup is a separate, auditable action.

## 10. Security

1. Authenticate every Git, snapshot, blob, and search request.
2. Authorize repository and revision access before resolving paths or exposing
   index-derived snippets.
3. Normalize URL paths at the gateway, but keep repository paths as validated byte
   strings; reject NUL and `/` within individual components.
4. Prevent repository identifiers and revisions from becoming filesystem or command
   arguments without typed validation.
5. Sandbox Git compatibility subprocesses with fixed environment, resource limits,
   no hooks, no arbitrary upload-pack path, and read-only repository access.
6. Bound decompression ratios, blob sizes, regex work, result counts, and concurrent
   FUSE requests.
7. Verify every cached object cryptographically before publication.
8. Partition caches and metrics so object existence is not leaked across tenants.
9. Audit ref resolution, file download, search, export, and push by job and user.
10. Default to safe symlink handling in hosted environments: reject absolute links
    and links that escape the mount, with an explicit compatibility opt-out.
11. Keep FUSE privilege in a trusted host/CSI component, not the agent container.

## 11. Performance targets

These are prototype acceptance targets to validate against representative
monorepos, not guaranteed SLOs:

| Metric | Target |
| --- | --- |
| Cold mount to usable root | under 2 seconds, under 10 MiB downloaded |
| Cached `getattr` | p95 under 1 ms client-side |
| Uncached root `readdirplus` | p95 under 250 ms in-region |
| Uncached source file first byte | p95 under 250 ms in-region |
| Warm literal search on an indexed branch | p95 under 2 seconds |
| Search-induced client blob hydration | zero bytes |
| `xvfs status` and shim `git status` after edits | p95 under 200 ms, no base metadata sweep |
| Client disk for a job | overlay + bounded shared cache, independent of repository history size |
| Crash recovery | no lost acknowledged overlay mutation in fault-injection tests |

The benchmark report must also compare network bytes, local disk, startup time, and
task completion time against full clone, `--filter=blob:none`, and sparse checkout.

## 12. Compatibility boundaries

MVP:

- Linux and FUSE3;
- case-sensitive paths;
- Git smart HTTP clone/fetch;
- SHA-1 repositories with the `files` ref backend, and algorithm-generic internal
  types from day one;
- whole-blob fetch;
- tracked files, directories, executable bit, and symlinks;
- writable overlay and patch export;
- literal and bounded regex search;
- a synthesized read-only `.git` plus the `git` shim, per section 8.6;
- single-node server storage.

The MVP also differs from a real checkout in ways that are correct by design but
must be stated, because they are invisible to the agent unless documented:

- **No Git content filters.** The mount serves raw blob bytes. `.gitattributes`
  `text`/`eol` conversion, `core.autocrlf`, and clean/smudge filters are not
  applied, so a repository that relies on them presents different bytes than
  `git checkout` would produce for the same commit.
- **LFS files appear as pointer files.** LFS content is not resolved in the MVP.
- **Base timestamps use the server's sanitized stable snapshot time.** Overlay
  timestamps cannot be set below the base-plus-one-tick floor, and base inode
  numbers are stable only within a mount generation, per section 8.2.
- **`reftable` repositories and SHA-256 repositories are rejected, not degraded.**
  libgit2 cannot read the former and its support for the latter is experimental.

Before production:

- SHA-256 repository conformance, which depends on libgit2 support maturing or on a
  narrow non-libgit2 path for object hashing; if neither lands, SHA-256 hosting
  stays out of scope and is declared rather than silently unsupported;
- crash-safe overlay migration;
- multi-replica server deployment;
- tokenized search if users need it;
- LFS pointer awareness;
- documented large-file behavior;
- push or a robust patch-application integration;
- CSI/host-daemon packaging;
- multi-node retention and derived-index garbage collection, extending the mount
  retention leases that the MVP already requires.

Submodules, special files, case-insensitive clients, refresh with local changes, and
cross-platform clients require separate designs.

## 13. Key risks and mitigations

| Risk | Consequence | Mitigation |
| --- | --- | --- |
| Agent runs raw `rg`/scanner | Entire snapshot hydrates | Agent tool, `xvfs-rg`, hydration telemetry and optional hard quota |
| FUSE unavailable in hosted container | Client cannot mount | Host daemon/CSI; validate in first spike; materialize fallback |
| Build expects uncommon POSIX semantics | Incorrect build | Declare boundary, run filesystem suites, add operations from measured failures |
| Search index not ready for arbitrary commit | Slow first search | Eager branch indexing, incremental manifests, on-demand operation with progress |
| Search omits overlay edits | Agent sees stale results | Merge server base results with local overlay search |
| Branch moves during API calls | Mixed snapshot | Resolve and pin commit once; require commit OID thereafter |
| libgit2 blocks an async worker | Request-pool starvation | Bounded blocking pool, request-local handles, deadlines, admission control |
| `upload-pack` process is abused | CPU/memory exhaustion or object leak | Repository-level authorization, sandbox, fixed config, quotas, timeouts |
| libgit2 and stock Git interpret a repo differently | Inconsistent API and clone views | Version matrix, shared fixtures, `git fsck`, ref/object conformance tests |
| Shared cache leaks tenant data | Security incident | Scope cache to trusted auth domain, strict permissions, optional encryption |
| Very large blobs dominate latency/disk | Poor job behavior | Size policy, whole-blob MVP, later chunk/range design |
| Index storage grows without bound | Server disk pressure | Blob deduplication, snapshot TTLs, ref retention, rebuildable generations |
| Overlay crash corrupts job | Lost work | WAL, atomic rename, fsync policy, model/fault tests |
| Pinned commit pruned by GC during a job | Mount breaks irrecoverably mid-task | Durable retention lease anchored as a ref; maintenance treats live mounts as roots |
| Lease expires while a job is alive | Mount loses its reachability root | Atomic mount creation, authenticated heartbeat renewal, grace interval, renewal alert |
| Internal lease ref is advertised or pruned | Old commits leak or live mounts break | Hide `refs/xvfs/`, reserve it from user revisions, exclude it from mirror/prune refspecs |
| Agent or build tool requires a working `.git` | Task fails or agent runs `git init` | Synthesized read-only `.git`, `git` shim, M0 measurement of the partial-clone option |
| Truncated search read as "not found" | Agent deletes or rewrites live code | Separate execution status and scoped coverage, missing-terminal failure, distinct truncation exit |
| Overlay search scans build outputs | `xvfs search` slower than plain `rg` | Ignore rules, binary/size classification, local time and byte budgets |
| libgit2 cannot open a target repository format | Repository unsupportable late in the project | Format validated at mirror creation; `reftable`/SHA-256 audited in M0 |
| Writable `MAP_SHARED` unsupported by the FUSE setup | Some linkers and tools fail | Measure in M0, document the boundary, enable writeback cache only if it is safe |
| Future-dated commit or skewed host clock | Build system treats edits as older | Sanitized snapshot time plus monotonic overlay logical clock |
| In-place refresh leaves stale kernel entries | Mixed old/new snapshot | Refresh by new mount generation and atomic bind-mount replacement |

## 14. Open questions

The prototype should answer these remaining questions with measurements:

1. Which hosted runtime will supply FUSE: direct container device, host daemon, or
   Kubernetes CSI?
2. What monorepos and agent workloads define success?
3. Is exact literal/regex search sufficient, or is ranked token search required?
4. What maximum acceptable delay should arbitrary-commit index preparation have?
5. Is patch export enough for the first integration, or must the client create and
   push commits?
6. Can a host cache be shared within a tenant, and what is the security boundary?
7. Are Git LFS, submodules, clean/smudge filters, and non-UTF-8 paths present in the
   target repositories?
8. Is the synthesized `.git` plus shim sufficient for the pilot's agents and build
   tooling, or does the measured cost of a shallow blobless partial clone justify
   real Git behavior?
9. Do any target repositories use the `reftable` backend or SHA-256, and is
   converting them on ingest acceptable?

## 15. References

- [Git protocol v2](https://git-scm.com/docs/protocol-v2) describes its
  command-oriented, stateless design, smart HTTP negotiation, filtering, and
  packfile URI capability.
- [Git partial clone design](https://git-scm.com/docs/partial-clone) describes
  promisor remotes, demand fetching, and the current cost of fetching missing
  objects.
- [Git pack format](https://git-scm.com/docs/gitformat-pack) documents on-disk and
  wire packs, delta representations, and SHA-1/SHA-256 object IDs.
- [VFS for Git](https://github.com/microsoft/VFSForGit) documents its virtualized
  working directory and specialized-service requirement.
- [Scalar](https://github.com/microsoft/scalar) documents its use of partial clone,
  sparse checkout, filesystem monitoring, commit graphs, and multi-pack indexes.
- [libgit2](https://github.com/libgit2/libgit2) is the repository implementation,
  used from Rust through [`git2-rs`](https://docs.rs/git2/latest/git2/).
- [`git-upload-pack`](https://git-scm.com/docs/git-upload-pack) documents the stock
  read-side Git service and its stateless HTTP modes.
- [`git-http-backend`](https://git-scm.com/docs/git-http-backend) documents the
  smart-HTTP request/response framing the gateway must reproduce, including the
  version-dependent `# service=` advertisement preamble and gzip-encoded request
  bodies.
- [Linux FUSE documentation](https://docs.kernel.org/filesystems/fuse/fuse.html) and
  [`cberner/fuser`](https://github.com/cberner/fuser) cover the kernel/userspace
  model and selected Rust interface.
- [Tantivy](https://github.com/quickwit-oss/tantivy), [`roaring`](https://docs.rs/roaring/latest/roaring/),
  and [`regex-automata`](https://docs.rs/regex-automata/latest/regex_automata/) are
  the proposed search primitives.
