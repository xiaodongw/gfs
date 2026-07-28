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
