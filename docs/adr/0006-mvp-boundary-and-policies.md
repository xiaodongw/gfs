# ADR 0006: MVP boundary, versioning, and the policies M1 implements

- Status: Accepted, with two items requiring product input
- Date: 2026-07-26
- Milestone: M0.6
- Evidence: ADRs 0001–0005 and the reports under `spikes/reports/`

## Context

M0.6 closes the milestone by answering DESIGN.md section 14's open questions,
freezing the MVP boundary and performance gates, and fixing the two policies M1
implements directly: mount retention leases and timestamps. Those two are here
rather than in DESIGN.md because M1.2 and M2.2 build them immediately, and a
policy discovered during implementation is a policy nobody reviewed.

## Answers to the open questions

| # | Question | Answer | Basis |
| --- | --- | --- | --- |
| 1 | Which runtime supplies FUSE? | Host daemon; direct in-container mounting only for trusted single-tenant deployments. CSI is the production form but is **unmeasured**. | ADR 0003 |
| 2 | Which monorepos and workloads define success? | **Unresolved — needs product input.** Public stand-ins (linux, rust, vscode) were used; the harness is parameterized by `corpus.conf`. | — |
| 3 | Is literal/regex enough, or is ranked token search needed? | Literal/regex is enough. Token search deferred out of MVP. | ADR 0004 |
| 4 | Max acceptable delay for arbitrary-commit index preparation? | **5 seconds** to `READY`, with an operation ID beyond that. A full manifest rebuild for the worst case measured 445 ms. | ADR 0004 |
| 5 | Is patch export enough for the first integration? | Yes for the pilot. **Confirm with the integration owner**; M8 moves earlier if not. | — |
| 6 | Can the host cache be shared within a tenant? | Yes, scoped to one repository, which is one authorization domain. | ADR 0002 |
| 7 | Are LFS, submodules, filters, non-UTF-8 paths present? | Submodules **yes** (rust: 12 gitlinks). `.gitattributes` text/eol conversion **yes** (vscode: 7 rules). Non-UTF-8 paths **no** at any corpus tip. LFS **not present** in the corpus. | M0.1, M0.3 |
| 8 | Is the synthesized `.git` sufficient? | Yes, with a mandatory shim and two corrections. | ADR 0005 |
| 9 | Do targets use `reftable` or SHA-256? | None in the corpus. Both rejected at ingest. | ADR 0001 |

Question 7's answer changes emphasis. Non-UTF-8 paths are **absent** from all
three corpus tips, so byte-path handling is insurance rather than a daily
concern — but it stays, because it is far cheaper to build in than to retrofit,
and the `bytes` fixture keeps it tested. Submodules and `.gitattributes`
conversion, by contrast, are **present**, so the gitlink representation and the
raw-bytes divergence are live compatibility issues for the pilot, not
hypotheticals.

## Frozen MVP compatibility boundary

Unchanged from DESIGN.md section 12 except where measurement forced a change:

**Added or sharpened:**

- SHA-256 is **out of scope**, not merely "before production". It is unreachable
  through `git2-rs` at any version currently published (ADR 0001), so the
  pre-production commitment cannot be met by libgit2 maturing alone.
- The synthesized `.git` has **six** entries, not four: `HEAD`, `packed-refs`,
  `config`, `gfs.json`, `objects/`, `refs/` (ADR 0005).
- `git ls-files` and `git diff` invoked **outside** the shim return empty output
  with exit 0 rather than failing. Documented limitation (ADR 0005).
- Repository read access implies read access to every object in that
  repository's object database, including unreachable and force-pushed-away
  commits (ADR 0002).
- A tag that does not dereference to a commit is rejected with a typed error
  rather than resolved (M0.3; Linux's `v2.6.11` peels to a tree).

**Unchanged:** Linux and FUSE3; case-sensitive paths; smart HTTP clone/fetch;
SHA-1 with the `files` ref backend; whole-blob fetch; raw blob bytes with no
content filters; LFS pointers unresolved; sanitized stable base timestamps;
single-node storage.

## Performance gates

Prototype acceptance targets, revised where M0 produced a real number:

| Metric | Target | Basis |
| --- | --- | --- |
| Cold mount to usable root | < 2 s, < 10 MiB | unchanged; probe mount was 18.7 ms |
| Cached `getattr` | p95 < 1 ms | M0.2 measured 0 upcalls for repeated stats |
| Uncached root `readdirplus` | p95 < 250 ms | unchanged |
| Uncached source file first byte | p95 < 250 ms | unchanged |
| Warm literal search | p95 < 2 s | unchanged |
| Search-induced client hydration | **zero bytes** | unchanged; the server never returns blobs to the client cache |
| `gfs status` after edits | p95 < 200 ms, no base metadata sweep | ADR 0005 makes "no sweep" the load-bearing half |
| Arbitrary-commit index preparation | < 5 s to READY | ADR 0004 (445 ms measured worst case) |
| Manifest storage per retained snapshot | < 5 MiB | ADR 0004 (1.99 MiB measured worst case) |
| Crash recovery | no lost acknowledged mutation | unchanged |

## API versioning and repository path semantics

- **gRPC/Protobuf**: additive-only within a major version. New fields optional
  with safe defaults; no field renumbering; no enum value repurposing. A removed
  field is reserved, never reused.
- **HTTP**: `/v1/...` path-versioned. A breaking change mints `/v2`; both serve
  during a deprecation window of at least two client releases.
- **Client/server skew**: a client must run against a server one minor version
  newer or older. CI tests the matrix.
- **Repository identity**: an opaque server-assigned `repository_id`, never a
  display name or filesystem path. Display names are mutable metadata.
- **Repository paths in URLs**: a single validated component matching
  `[A-Za-z0-9._-]{1,128}`, not starting with `.` and not containing `..`. Never
  concatenated into a filesystem path before validation. The M0.3 gateway probe
  tests ten traversal and absolute-path forms against this rule.
- **Byte paths**: `path_b64url` on the wire; the protocol carries bytes, and no
  layer round-trips a path through `String`.
- **Object IDs**: always `{algorithm}:{hex}`. A bare hex digest is accepted only
  where a repository context supplies the algorithm.
- **Reserved namespace**: `refs/gfs/` is rejected as a user-supplied revision
  at the lowest layer, so no caller can forget to.

## Mount retention-lease policy

Fixed here because M1.2 implements it directly.

| Parameter | Value | Reasoning |
| --- | --- | --- |
| Initial TTL | 30 minutes | Long enough that a brief control-plane outage is survivable; short enough that an orphan is reclaimed within a shift. |
| Heartbeat interval | 5 minutes | Six renewals per TTL, so five consecutive failures are tolerated before grace. |
| Renewal grace after expiry | 15 minutes | A transient failure gets reported and recovered before a live workspace is destroyed. |
| Prune delay after grace | 24 hours | Objects stay recoverable for a working day after a mistaken expiry. |
| Maximum total lease age | 24 hours | An abandoned daemon that renews forever is still bounded. Renewal past this fails and alerts. |
| Alert threshold | 2 consecutive renewal failures | Fires roughly 10 minutes before grace begins. |

Mechanics, unchanged from DESIGN.md section 7.1 and confirmed implementable in
M0.3: create under the repository lock as `PREPARING` → durable ref anchor →
`ACTIVE`, with restart reconciliation removing abandoned `PREPARING` anchors.
libgit2 ref transactions were verified to create and remove anchors visibly to
stock Git, leaving no trace on rollback.

`refs/gfs/` is hidden from advertisement, rejected as a user revision, and
excluded from every upstream fetch and prune refspec. **It is not an
authorization mechanism** — ADR 0002 measured that hiding prevents discovery,
not access.

## Timestamp policy

Unchanged from DESIGN.md section 8.2, with the acceptance cases fixed:

```text
snapshot_time = clamp(committer_time, minimum_supported_time,
                      authoritative_first_seen_time - one_tick)
new_overlay_time = max(host_wall_clock, snapshot_time + one_tick,
                       prior_entry_time + one_tick)
```

- `minimum_supported_time` = 1990-01-01T00:00:00Z. Earlier committer timestamps
  exist and break build systems that treat them as clock skew.
- `one_tick` = the mount's reported timestamp resolution (1 ns with
  `nsec` support, which the pinned libgit2 build has).
- The catalog writer supplies `authoritative_first_seen_time`; replicas reuse
  the stored value and never recompute it from their own clock.

Acceptance cases M2.2 must cover: a future-dated commit; a host clock skewed
backwards past `snapshot_time`; two writes within one tick; an explicit
`utimensat` below the overlay floor (clamped, and `ctime` still advances); base
timestamps identical across two remounts and two hosts.

## Threat model: host cache and FUSE privilege

**Cache.** Keyed by `(repository_id, algorithm, oid)`; content verified as a
canonical Git blob before publication; written to a temporary file and atomically
renamed. Deduplication is scoped to one repository — one authorization domain
per ADR 0002 — so cross-tenant plaintext dedup, and the content-existence side
channel it creates, does not arise. Cache directories are mode 0700 owned by the
daemon; jobs reach content only through the mount.

**FUSE privilege.** The daemon runs unprivileged on the host and needs no
capabilities (M0.2). The job container receives a bind mount and needs neither
`/dev/fuse` nor `CAP_SYS_ADMIN`. `user_allow_other` in `/etc/fuse.conf` is a
privileged one-time host prerequisite. The control socket is mode 0600 and
carries the job's scoped capability, not repository credentials.

**Residual risks, accepted and recorded:**

- A repository reader can fetch any object in that repository by OID over
  protocol v2, including lease-retained commits (ADR 0002).
- The shim and `gfs rg` are `PATH`-based conveniences, not boundaries. A
  program that opens every file still hydrates every file unless a hard budget
  stops it.
- Tools bypassing the shim get silently-empty `ls-files` and `diff` (ADR 0005).

## Failure-mode and data-retention policy

| Failure | Behaviour |
| --- | --- |
| Server unreachable, uncached base read | retryable `EIO`; overlay and cached reads continue |
| Daemon killed with open descriptors | `ENOTCONN`; mount stays until explicitly unmounted (M0.2) |
| Lease renewal failing | warn at 2 failures, `EIO` on uncached reads after grace, never silent |
| Search backend partial failure | non-success terminal status; never an empty result (ADR 0004) |
| Overlay disk full | `ENOSPC` on new writes; existing overlay content preserved |
| Unmount with open files | `EBUSY`; job cleanup is a separate auditable action |

**Retention.** Overlay state survives unmount and is deleted only by explicit
job cleanup, which is audited. Exports are retained per the orchestrator's
artifact policy. Audit records carry repository, commit, subject, and job — never
file content, tokens, or unvalidated paths. Derived data (manifests, decoded
blobs, index generations) is rebuildable from Git and may be deleted at any time.

## Items requiring product input

These two are **not** engineering decisions and are recorded as open:

1. **Which monorepos and agent workloads define success** (question 2). The
   corpus is public stand-ins. Every measurement in M0 is reproducible against
   the real repositories by editing `spikes/corpus/corpus.conf`, but the
   go/no-go's "materially lower cost" claim is only as representative as the
   corpus behind it.
2. **Whether patch export suffices for the first integration** (question 5). If
   the review pipeline requires real commits, M8 moves ahead of M7 and the
   critical path lengthens.

## Amendment, 2026-07-26: mmap and the FUSE writeback cache

PLAN.md M2.3 left one question open: "Determine whether writable `MAP_SHARED` is
available in the target deployment and whether enabling the writeback cache to
get it is acceptable; record the answer as a compatibility boundary either way."
Measured in `gfs-fuse/tests/mmap.rs` on Linux 6.18.33.2 (WSL2).

| Mapping | Against | Result |
| --- | --- | --- |
| `MAP_PRIVATE`, `PROT_READ` | the GFS mount | works |
| `MAP_SHARED`, `PROT_READ` | the GFS mount, 4 MiB file | works |
| `MAP_SHARED`, `PROT_READ\|PROT_WRITE` | the GFS mount | `EROFS` at `open(2)`, before `mmap` |
| `MAP_SHARED`, `PROT_READ\|PROT_WRITE`, **no** `FUSE_WRITEBACK_CACHE` | a writable probe filesystem | **works**; one `write` request; dirtied bytes reach the filesystem |
| `MAP_SHARED`, `PROT_READ\|PROT_WRITE`, **with** `FUSE_WRITEBACK_CACHE` | the same probe | works; capability granted |

The write side needed a second filesystem because GFS refuses a read-write
`open`, so a writable mapping never reaches `mmap`. The question is a property of
FUSE, not of GFS, and `tests/mmap.rs` mounts a one-file in-memory probe twice to
answer it.

### Decision

**Writable `MAP_SHARED` is supported, and GFS does not enable
`FUSE_WRITEBACK_CACHE`.**

The capability is not needed for it — the mapping works without it on this
kernel — and enabling it would be actively harmful here. `FUSE_WRITEBACK_CACHE`
transfers ownership of `size` and `mtime` to the kernel. This ADR's own
timestamp policy requires the opposite:

```text
new_overlay_time = max(host_wall_clock, snapshot_time + one_tick,
                       prior_entry_time + one_tick)
```

That formula exists so an acknowledged local edit is strictly newer than the base
even for a future-dated commit or a host clock skewed backwards, and it is only
enforceable if the daemon assigns `mtime`. A kernel-assigned `mtime` from the
host clock is precisely the skew case the policy was written to survive.

So the two requirements do not conflict: the one that would have forced writeback
on us turns out not to need it.

### Consequences

- M3.2 implements mutation with the daemon owning `size` and `mtime`, and must
  not enable `FUSE_WRITEBACK_CACHE` to "fix" a write-performance problem without
  revisiting the timestamp policy first.
- The compatibility boundary gains a line: **read-only `mmap` is supported;
  writable `MAP_SHARED` is supported from M3 on overlay files, and remains
  `EROFS` on base files for the life of the MVP** — the base is an immutable
  commit, so there is nothing for a shared writable mapping of it to mean.
- **Scope caveat, the same one ADR 0003 carries.** This is one kernel. The
  hosted runner is unmeasured, and `tests/mmap.rs` should be re-run there
  alongside `spikes/fuse-probe/deployment-matrix.sh` before M6.1. If a target
  kernel refuses the mapping without writeback, the decision above has to be
  reopened *together with* the timestamp policy, not separately.
