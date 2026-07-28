# M0.2 — FUSE deployment spike

Milestone exit gate (PLAN.md M0.2):

> a documented deployment path works in the actual hosted environment.

**Met for WSL2 and Docker. Not met for Kubernetes** — no cluster was reachable
from this machine, so the CSI node-plugin leg is unmeasured. That scope limit
applies to any M0 go/no-go claim about hosted deployment.

Decision: [ADR 0003](../../docs/adr/0003-fuse-deployment-model.md).

## How to reproduce

```sh
cd spikes
cargo build -p fuse-probe
./target/debug/fuse-probe capabilities
./target/debug/fuse-probe measure --dir /tmp/gfs-probe --files 64 \
    --file-size 65536 --latency-ms 20 --parallel 16
docker build -t gfs-fuse-probe:latest fuse-probe/
./fuse-probe/deployment-matrix.sh
```

Machine: WSL2, Linux 6.18.33.2-microsoft-standard-WSL2, 32 cores, 46 GiB RAM,
uid 1000, Docker 29.1.3, fusermount3 present, `user_allow_other` **not** set.

## Where a mount is possible

| Environment | Privilege | Result |
| --- | --- | --- |
| Host, ordinary user | none | **mounts** |
| Container | `--device /dev/fuse --cap-add SYS_ADMIN` | **mounts** |
| Container | `--device /dev/fuse` only | `fusermount3: mount failed: Operation not permitted` |
| Container | none | cannot mount, as designed |

`/dev/fuse` alone is not enough in a container: Docker's default
seccomp/AppArmor profile denies the `mount(2)` behind `fusermount3`. In-container
mounting costs `CAP_SYS_ADMIN`, which is not acceptable for a container running
untrusted agent code. On the host the same mount needs no privilege at all.

## The `allow_other` prerequisite

| Case | Result |
| --- | --- |
| Different UID reads the mount, `allow_other` set | **works** |
| Different UID reads the mount, default | `EACCES` |
| `allow_other` requested by a non-root mounter | needs `user_allow_other` in `/etc/fuse.conf` |
| `auto_unmount` requested | requires `allow_other`, so the same prerequisite |
| dockerd bind-mounting a uid-1000 FUSE mount | **blocked** without `allow_other` |

The last row is the one worth knowing about in advance. The Docker daemon runs
as root, cannot traverse a uid-1000 FUSE mount without `allow_other`, and fails
while *preparing* the bind source with
`error while creating mount source path ...: file exists` — a message that
points nowhere near the actual cause.

`user_allow_other` could not be set on this machine (no root), so the full
host-daemon bind-mount path is proven by mechanism rather than end to end: the
cross-UID read was demonstrated inside a container where the probe had root and
could set it.

## Dispatch model

64 files, 16 reader threads, 20 ms origin latency per fetch:

| Dispatch | FUSE threads | Wall time | Peak concurrent fetches |
| --- | --- | --- | --- |
| Blocking | 1 | 1321 ms | 1 |
| Blocking | 8 | 170 ms | 8 |
| Pooled | 1 | 123 ms | 16 |

1321 ms is exactly the serial cost (64 × 20 ms): one blocking callback thread
makes the whole mount sequential. DESIGN.md section 8.2's rule is confirmed
quantitatively, and `n_threads` alone is not a substitute — it caps concurrency
at the thread count, while pooled dispatch reaches full reader concurrency from
a single event-loop thread.

> **Measurement note.** The first version of this benchmark reported peak
> concurrency 16 with a 1300 ms wall time in every configuration. The origin
> client held one connection behind a mutex, so threads blocked on the lock were
> counted as concurrent fetches. The probe was measuring its own lock. Fixed by
> giving each fetch its own connection; the numbers above are from the corrected
> version. Recorded because the failure mode — a concurrency metric that rises
> while wall time does not — is easy to accept as a win.

## Other measurements

| Property | Result |
| --- | --- |
| Mount to usable mount point | 18.7 ms (target: under 2 s) |
| 1000 `stat(2)` at a 60 s attribute TTL | **0** getattr upcalls |
| Warm re-read of 64 files | 6.0 ms, 0 origin fetches |
| Root `readdir` of 58 entries | 0.69 ms |
| Write to a base file | `EROFS`, as intended |
| `statfs` | reports the notional overlay quota, not the host filesystem |

## Failure semantics

| Event | Behaviour |
| --- | --- |
| Daemon `SIGKILL`ed with a file open | fd fails with `ENOTCONN` |
| Any operation on the orphaned mount | `ENOTCONN` until explicitly unmounted |
| Cleanup of an orphaned mount | needs an explicit `fusermount3 -u` |
| Unmount with an open file | refused, `EBUSY` |

Failing loudly with `ENOTCONN` is the safe direction: a build breaks rather than
silently reading short. Orphan cleanup is an orchestrator responsibility, which
PLAN.md M6.1 already assigns; `auto_unmount` would move it to the kernel at the
cost of requiring `allow_other`.

## Limitations

- **Kubernetes/CSI is unmeasured.** The largest gap in this milestone.
- `user_allow_other` could not be set on the host, so the end-to-end
  host-daemon bind mount into an unprivileged container is proven by mechanism,
  not by a single end-to-end run.
- WSL2 is not a production kernel. Container behaviour here is Docker's default
  profile on a Microsoft kernel; the real runner must be re-tested.
- The probe serves a tiny synthetic tree. Metadata behaviour at monorepo scale
  (a million-entry snapshot, deep paths, huge directories) is M2.4's scope.
- `mmap`, writable `MAP_SHARED`, and the writeback-cache question in DESIGN.md
  section 8.2 are not covered here; they belong with the M2.3 read path.
