# ADR 0003: FUSE deployment model and the FUSE dispatch rule

- Status: Accepted; the CSI leg is deliberately deferred (see the amendment below)
- Date: 2026-07-26
- Milestone: M0.2
- Evidence: `spikes/fuse-probe`, `spikes/reports/m02-fuse-deployment.md`

## Context

DESIGN.md section 8.1 proposes running `gfs-fuse` as a trusted host service or
Kubernetes CSI node component and bind-mounting a per-job workspace into the
unprivileged agent container, with direct in-container mounting "supported only
on platforms that safely expose FUSE". M0.2 was to decide between direct mount,
host daemon, and CSI with measurements.

Measured on WSL2 (Linux 6.18.33.2) with Docker 29.1.3. **No Kubernetes cluster
was reachable, so the CSI leg is unmeasured** — see Consequences.

## Decision

### 1. Host daemon, not in-container mounting

| Configuration | Result |
| --- | --- |
| Host, unprivileged uid 1000, no capabilities | **mounts** |
| Container, `--device /dev/fuse --cap-add SYS_ADMIN` | **mounts** |
| Container, `--device /dev/fuse` only | fails: `fusermount3: mount failed: Operation not permitted` |
| Container, no device and no capabilities | fails, as designed |

`/dev/fuse` alone is not sufficient inside a container: Docker's default
seccomp/AppArmor profile denies the `mount(2)` that `fusermount3` performs, so
in-container mounting costs `CAP_SYS_ADMIN`. Granting `CAP_SYS_ADMIN` to a
container running untrusted agent code is close to granting root on the host,
which is exactly what the hosted-agent threat model must not do.

On the host, by contrast, an ordinary unprivileged user mounts with no
capabilities at all. **The privilege asymmetry is the whole argument**: the
daemon needs no privilege where it runs, and the job needs no privilege to use
what the daemon produced.

### 2. `allow_other` is a hard prerequisite, and it is a host-level action

The bind-mount model only works if a process with a different UID can read the
mount. Measured:

- with `allow_other`: a different UID reads the mount — **works**;
- without it: a different UID gets `EACCES` — **as documented**;
- `allow_other` for a non-root mounter requires `user_allow_other` in
  `/etc/fuse.conf`, a privileged one-time host configuration;
- `auto_unmount` additionally requires `allow_other`, so crash cleanup depends
  on the same host configuration.

This surfaced one layer earlier than expected: the Docker daemon itself runs as
root and could not even *prepare* a bind mount whose source was a uid-1000 FUSE
mount without `allow_other`, failing with an opaque
`error while creating mount source path`. An operator who has not set
`user_allow_other` will see that message, not a permissions error.

**`user_allow_other` in `/etc/fuse.conf` is therefore a documented deployment
prerequisite of the host-daemon model, alongside `/dev/fuse` and `fuse3`.**

### 3. Blocking in a FUSE callback serializes the entire mount

DESIGN.md section 8.2 asserts that blocking work must not run on a callback
thread. Measured, with 64 files, 16 reader threads, and 20 ms of origin latency:

| Dispatch | FUSE event-loop threads | Wall time | Peak concurrent fetches |
| --- | --- | --- | --- |
| Blocking | 1 | **1321 ms** | 1 |
| Blocking | 8 | 170 ms | 8 |
| Pooled (reply from a worker) | 1 | **123 ms** | 16 |

1321 ms is exactly the serial cost, 64 × 20 ms. One blocking callback thread
turns a parallel build into a sequential one.

Both remedies work, and they are independent. The rule adopted is **both**:
`n_threads` > 1 *and* never blocking a callback. `n_threads` alone caps
concurrency at the thread count, and it is the pooled model that reaches 16
concurrent fetches from a single event-loop thread. `fuser` 0.18 makes this
practical — the reply handles are `Send`, so a callback can hand `ReplyData` to
a worker and return immediately.

### 4. Failure semantics are usable but need explicit cleanup

- **Daemon killed with an open fd**: the file descriptor fails with `ENOTCONN`
  ("Transport endpoint is not connected"). It fails loudly rather than reading
  short or returning zeros, which is the safe direction — a build breaks instead
  of silently producing a wrong artifact.
- **The mount point stays** after the daemon dies, and every operation on it
  returns `ENOTCONN` until something calls `fusermount3 -u`. Orphan cleanup is
  therefore an explicit responsibility of the orchestrator, not something the
  kernel does. `auto_unmount` would delegate it to the kernel, at the cost of
  requiring `allow_other`.
- **Unmount with an open file** is refused with `EBUSY`, so job teardown must
  either close descriptors first or use a lazy unmount.

### 5. Kernel caching makes the metadata path viable

1000 `stat(2)` calls on one path produced **0** `getattr` upcalls at a 60-second
attribute TTL. Because the base commit is immutable, long TTLs are correct, and
the kernel absorbs repeated metadata traffic completely. This is what makes
`ls -l` over a monorepo affordable, and it is the mechanism the M2 performance
targets depend on.

Mount to a usable mount point took 18.7 ms, against a 2-second target.

## Alternatives considered

**Direct in-container mount.** Rejected: costs `CAP_SYS_ADMIN` in the container
running untrusted code. Keep it as a supported option only for trusted,
single-tenant deployments, and never as the hosted-agent default.

**Kubernetes CSI node plugin.** Not rejected — unmeasured. It is the natural
production form of the host-daemon model and DESIGN.md already anticipates it.
The measurements here transfer to it (a CSI node plugin is a privileged host
component that publishes a mount into an unprivileged pod), but "transfer" is
an argument, not a measurement.

**`gfs materialize` as the primary path.** Not needed. Every environment tested
can mount somewhere. PLAN.md M6.1 says to remove `materialize` from the design
if every target environment can mount; that decision cannot be finalized until
the real hosted runner is tested.

## Consequences

- The M0.2 exit gate — "a documented deployment path works in the actual hosted
  environment" — is **met for WSL2 and Docker and not met for Kubernetes**. Any
  go/no-go statement about hosted deployment must carry that scope. Re-running
  `spikes/fuse-probe/deployment-matrix.sh` against the real runner is a
  prerequisite for the M6 pilot, and it is cheap.
- M1.1's local development stack and M2.1's host-daemon skeleton both inherit
  the `user_allow_other` prerequisite; it belongs in the installation
  documentation, not in a troubleshooting appendix.
- M2.3's blob-fetch design must not block a callback thread. The measurement
  above is the acceptance test for that rule.
- Orphan-mount cleanup is an orchestrator responsibility (PLAN.md M6.1 already
  lists it). The `ENOTCONN` behaviour means a leaked mount is visible and
  inert rather than silently wrong.

## Amendment, 2026-07-26: the CSI measurement is deferred until the prototype works locally

Decided after M1 completed. This changes *when* the unmeasured leg gets measured,
not what the ADR concluded.

### Decision

**Defer re-running `spikes/fuse-probe/deployment-matrix.sh` on a real hosted runner
and on Kubernetes until the prototype mounts and serves a workspace locally.** The
trigger is M2 complete — a working local mount — not a date.

The M0 go/no-go listed this as condition 1 and suggested "ideally before M2 commits
to the host-daemon skeleton". That ordering is dropped.

### Why deferring is cheap

The two things that could make this expensive are both absent:

1. **It is currently unrunnable rather than unrun.** The script has already run on
   this machine; the gap is specifically Kubernetes and the real hosted runner, and
   neither is reachable. Re-running it here would reproduce the tables above.
2. **The unmeasured leg does not constrain M2's code.** Everything M2 builds on is
   measured for WSL2 + Docker: the unprivileged host mount, the pooled-dispatch rule
   (1321 ms → 123 ms), the 0-upcall metadata caching, `ENOTCONN` on daemon death, and
   `EROFS` on a base write. What CSI would change is how a mount is *published* to a
   job — a packaging concern owned by M6.1 and M7.4 — not the inode model, the blob
   cache, or the `.git` surface.

The residual risk is that M2's host-daemon skeleton hardens around Docker-shaped
assumptions and has to be reworked. That is mitigated by design rather than by
measurement: see below.

### What M2 must do to keep the deferral cheap

- **Keep mount publication behind one seam.** The M2.1 skeleton should treat "make
  this mount visible to the job" as a single replaceable step. A CSI node plugin and
  a host daemon differ only in that step, so a later answer must not ripple into the
  filesystem code.
- **Do not assume the daemon and the job share a UID, and do not require that they
  differ.** M2's own tests mount and read as the same UID (see below); the
  cross-UID path stays exercised by ADR 0003's existing measurements until M6.1
  needs it for real.

### `user_allow_other` and M2's tests

`user_allow_other` is still unset in `/etc/fuse.conf` on the development host, so
`dockerd` cannot stat a uid-1000 FUSE mount and the `container_bind_mount_from_host`
case remains `BLOCKED` here.

**M2's tests therefore mount and read as the same UID.** That is sufficient for
everything M2 is accountable for — POSIX conformance, the blob cache, the timestamp
rule, and the `.git` surface — none of which depends on cross-UID access. Requiring
`user_allow_other` for the ordinary test suite would make a privileged host action a
prerequisite for running `cargo test`, which is a worse trade than deferring one
integration path.

The prerequisite itself is unchanged and still belongs in the installation
documentation: the bind-mount model does not work without it, and section 2 above is
the measurement that says so.

### The trigger has fired, 2026-07-26

M2 is complete: the prototype mounts and serves a workspace locally
([M2 report](../reports/m2-completion.md)). The deferral's condition is therefore
met and re-running `spikes/fuse-probe/deployment-matrix.sh` on the real hosted
runner and on Kubernetes is unblocked. What M2 owed in exchange was delivered:
mount publication is behind one replaceable step (`crates/gfs-mount/src/publish.rs`),
and M2's own tests mount and read as the same UID.

### When this stops being deferrable

Before M6.1. The pilot's orchestrator bind-mounts a workspace into an unprivileged
container, which is exactly the path that is `BLOCKED` locally and unmeasured on
Kubernetes. `gfs materialize` also cannot be resolved until then, for the reason
already given under Alternatives.

## Amendment, 2026-07-28: the workspace is the mount point, and a re-pin happens in place

Decided after using the prototype with an agent CLI. This replaces M2's
generation model. It does not touch decisions 1–5 above, and it *keeps* the one
thing the deferral was bought with: publication is still a single replaceable
step.

### What the generation model cost

M2 published a workspace as a symlink to `<workspace>.gfs/generations/<n>`, and
made `gfs refresh` create a whole new generation — a new `CreateMount`, a new
FUSE session at a new mount point — rather than mutate a live one. The guarantee
it bought is that no reader ever observes a mixture of two commits.

The cost is `getcwd(2)`. The kernel resolves a working directory from its
dentries, so it returns the *physical* path; only a shell keeps a logical `$PWD`
of its own. Every other tool — anything calling `getcwd`, `realpath`, or
`canonicalize` at startup, which is essentially every non-shell process —
therefore captures `.../generations/1` and holds it. The next switch retires that
generation underneath the tool, and every subsequent operation fails with
`ENOENT`.

That breaks the workflow the project exists to serve:

```
start an agent in a workspace  ->  ask it to switch branch  ->  keep working
```

which is ordinary in Git and was impossible here.

### The guarantee was stronger than Git's

`git switch` rewrites a working tree in place, file by file. Mid-checkout the
tree genuinely is a mixture; a process holding a descriptor gets whatever the
kernel gives it; a process whose working directory the new branch does not have
gets `ENOENT`. Git offers no coherence guarantee at all, and no tooling expects
one.

So M2's guarantee was not merely expensive, it was *unwanted*: it made the
workspace behave unlike the thing it is meant to be a drop-in for.

### Decision

**1. `DirectMountPublisher` mounts the FUSE session at the workspace path
itself.** `gfs clone <url> <dir>` produces a mount at `<dir>`, and `getcwd`
inside it returns `<dir>`. A workspace that exists and is non-empty is refused at
construction, because mounting over it would hide its contents for the life of
the job rather than fail.

**2. `gfs switch`, `gfs refresh`, and the re-pin after `gfs commit` swap the
filesystem's pinned commit in place.** No second session, no second mount point,
no second lease held open. `Gfs::repin` replaces the client, `.git` surface,
overlay, and snapshot time as one value, and the mount point never moves.

**3. The kernel is told, not left to time out.** After the swap, every dentry the
mount handed out is invalidated with `FUSE_NOTIFY_INVAL_ENTRY`, off the session
threads — issuing it from inside a callback deadlocks the mount against itself.
The set is bounded by the paths the job actually touched, which is what the inode
table's `by_path` already records.

### What is kept, and what is given up

Kept, because it turned out not to need the generation model at all:

- **An open descriptor keeps reading what it opened.** A `FileState` holds an
  open handle on a materialized cache file or overlay content file and never
  re-reads through the client, so this costs nothing and depends on no lease.
  That is also why the superseded lease is released immediately rather than kept
  warm — the reason M2 kept it alive does not exist.
- **Inode numbers.** DESIGN.md section 8.2 promises a path keeps its number for
  the life of the mount, and a re-pin does not end the mount. `by_path` is not
  touched, so the stale-`(device, inode)`-hit hazard that section is about does
  not reappear.

Given up, in both cases matching `git switch`:

- A job whose working directory exists on the old commit and not on the new one
  gets `ENOENT` afterwards.
- A path the new commit *adds* may read as absent for up to `FsConfig::negative_ttl`
  (1 second). A negative dentry is not enumerable, so unlike a positive one it
  cannot be invalidated, only waited out. This is the one window with no Git
  equivalent, because Git rewrites the tree through the kernel rather than
  underneath it.

Both are asserted in `crates/gfs-mount/tests/lifecycle.rs` rather than left to
the reader — including the negative-TTL window, which is tested as a bounded
property instead of being hidden.

### The seam survives

`MountPublisher` keeps its purpose and gains `mountpoint()`: the publisher now
says *where the daemon should mount* as well as how that becomes visible. The
local implementation answers "the workspace" and makes `publish` a no-op; the
bind-mount and CSI publishers M6.1 and M7.4 need will answer with a private path
and `move_mount(2)` it onto the workspace. That is a better-shaped seam than the
symlink version, because it is the shape the privileged implementations actually
have.

The exchange this ADR's first amendment asked M2 for is therefore still honoured.

### Consequences

- PLAN.md M2.1's exit criterion is restated, not dropped: a re-pin keeps the
  workspace path and open descriptors, rather than isolating two live
  generations.
- `retire_timeout`, `retiring_generations`, and the `generations/` directory are
  gone. `--retire-timeout-seconds` is removed from `gfs-fuse`.
- `MountState.generation` survives as a re-pin counter. It is reported and it
  names the overlay directory; nothing is kept alive alongside it.
- Startup unmounts a stale mount at the workspace, and still sweeps a legacy
  `generations/` tree, so upgrading over a lab directory needs no manual cleanup.
- `gfs unmount` leaves the workspace as an empty directory rather than removing
  it. `umount` leaving its mount point behind is the Unix norm, and removing a
  directory a process may be standing in is worse than leaving an empty one.
  `mount.json` is the evidence of release.
- The invalidation sweep is the new cost of a switch, and it is proportional to
  what the job touched. A job that walked a monorepo pays for that walk again at
  the next switch. If that ever matters, the fix is to invalidate only the
  subtree the diff touched — the manifest diff already knows it.

## Amendment, 2026-09-02: a callback may complete on the FUSE thread

[ADR 0014](0014-answer-on-the-fuse-thread-and-let-the-kernel-keep-it.md)
refines the dispatch rule. The first half stands: a callback never blocks.
The second half — the work runs on the runtime — was implemented as "spawn
every handler and reply from a worker", which made a cache hit cost a thread
hop and a wait. Each handler's future is now polled once on the FUSE thread
inside the runtime's context and handed to the runtime only if it returns
pending. The measurement in section 3 still governs what may run in that
synchronous prefix: nothing that waits on I/O.
