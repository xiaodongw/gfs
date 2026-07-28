# ADR 0008: One `gfs-fuse` host, many mounts

- Status: Accepted
- Date: 2026-07-28
- Milestone: M3
- Supersedes: the one-mount-per-process rule asserted in `gfs-fuse`'s module
  documentation, which cited ADR 0006's failure policy as its justification

## Context

Every `gfs clone` forked a `gfs-fuse` process that owned exactly one mount. Two
clones meant two processes, two gRPC channels, two blob caches over the same
cache root, and two sets of FUSE event-loop threads.

The rule was deliberate and the reason was recorded: a daemon owning several
mounts has to decide what a *partial* failure means — one mount lost, the others
alive — and ADR 0006's failure policy has no answer for that. One process per
mount made "the daemon died" and "the mount is gone" the same event, which is a
thing an orchestrator can reason about.

Two things changed the balance. First, the hosted shape is many workspaces per
machine, so the per-process cost is paid per *job* rather than per host. Second,
the per-mount blob cache turned out to be a correctness problem and not only an
overhead one: `--cache-quota` silently meant "per mount", so N mounts of one
repository could use N times the configured quota and re-download the same blob
N times.

## Decision

### 1. One host process, still one control socket per mount

`gfs-fuse` becomes a *host*. It binds one host socket that answers
`CreateMount`, `ListMounts`, `DestroyMount`, `Info`, and `Shutdown`. Each mount
it creates is still a whole `Mount` with its own lease, its own generations, and
its own control socket at `<workspace>.gfs/control.sock`.

**The per-mount socket is kept, and that is what makes this change small.** It
was never an operating-system requirement: the kernel talks to a FUSE filesystem
over a `/dev/fuse` descriptor obtained by `fusermount3` and held in the process's
file table, and it never opens the control socket. The control socket is a
*name*, and one process can bind as many names as it likes.

Consolidating them into a single mount-addressed endpoint would have meant a
mount selector on all fifteen `Request` variants, a new discovery rule to replace
the upward walk that makes `gfs rg` work with no flags, and changes to every CLI
call site and to `gfs-git-shim` — for no capability the host does not already
have. Nothing that speaks `Request` can tell how many mounts its process owns.

### 2. The blob cache is shared per `(cache root, repository)`

The host keeps the registry and hands the same `Arc<BlobCache>` to every mount of
one repository, weakly, so a repository nobody mounts stops costing an in-memory
index. `--cache-quota` now means what it says.

FUSE threads are **not** shared: `fuser` spawns its own per session, so N mounts
is still N × `--fuse-threads`. Consolidation buys one runtime, one gRPC channel
per mount rather than per process (channel sharing is not yet done — see
Consequences), one supervised process, and the cache fix.

### 3. One host per socket, enforced by an `flock`

The host holds an exclusive `flock` on `<socket>.lock` for its lifetime, and only
then checks liveness, removes a stale socket, and binds. A pid file would need a
staleness rule and every staleness rule is wrong for a reused pid; the kernel
releases an `flock` when the process dies by any means.

Racing `gfs clone` invocations are expected: both may spawn a host, the loser
fails to take the lock and exits, and both then find the winner's socket. The CLI
therefore re-checks liveness after a child exits before reporting a failure.

### 4. `CreateMount` answers only once the mount's socket is bound

`Mount::bind_control` is separate from `Mount::serve_control` so the host can bind
synchronously and serve on a task. This closes a race that existed under the old
model: `gfs mount` wrote its ready file and then sent `Inspect` to a socket a
spawned task had not bound yet. The test harness polled `is_live` to paper over
it; it no longer needs to.

### 5. Version skew is refused at `CreateMount`

A long-lived host is exactly where a rebuilt CLI meets an old daemon.
`MountRequest` carries `state_format_version` and the host refuses a mismatch
with `FAILED_PRECONDITION`, naming `gfs daemon stop` as the fix. Serving it would
write a state directory the client cannot read back.

## Alternatives considered

**Keep one process per mount.** Rejected: it is what makes `--cache-quota` a lie,
and the failure-isolation argument for it is weaker than it looks (see below).

**One mount-addressed control socket.** Rejected as described in Decision 1: a
protocol change with no capability behind it. It remains available later without
redoing any of this work.

**A pid file for the singleton.** Rejected: staleness rules and pid reuse.

**Idle-exit for the host.** Not taken. The host lingers, and `gfs daemon stop`
stops it. An idle timeout races with a concurrent `gfs clone`, and the hosted
shape wants a long-lived supervised process anyway. `scripts/dev-server.sh` stops
the host on exit only when it has no mounts left, so a developer's workspaces
outside the lab survive.

## Consequences

- **The failure blast radius grew, and this is the real cost.** One OOM, one
  abort, or one `SIGTERM` now ends every mount the host owns, where before it
  ended one. What is *retained* is containment of ordinary faults: every mount's
  state is per-instance, so a poisoned lock, a failing lease, or a control socket
  that stops serving is confined to its workspace, and a mount whose control task
  ends for any reason is torn down and deregistered rather than left half-alive.
  ADR 0006's failure policy still has no statement about partial host failure;
  what changed is that the code no longer pretends the question cannot arise.
- **An orchestrator's `SIGTERM` is now a fleet-wide action.** Anything that
  supervised a per-job `gfs-fuse` has to move to `DestroyMount` on the host socket
  or `gfs unmount` on the workspace. Both tear down exactly one mount.
- **Logs are shared.** Every mount-scoped task runs inside a
  `tracing::info_span!("mount", workspace = …)`; under one process per mount the
  process itself was the answer and the fields were absent.
- **gRPC channel sharing is not done.** Each mount still opens its own channel to
  the gateway. `SnapshotClient::connect` builds the channel internally, so sharing
  means threading a pre-built channel through it. Worth doing, not required for
  this change.
- **The CSI question is unchanged.** ADR 0003's deferred Kubernetes measurement
  and its `publish.rs` seam are untouched: a host publishes mounts through the
  same single replaceable step a per-mount daemon did.
