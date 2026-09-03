# ADR 0015: Let the kernel read and write the files itself

- Status: Accepted (implemented 2026-09-03)
- Date: 2026-09-03
- Amends: [ADR 0014](0014-answer-on-the-fuse-thread-and-let-the-kernel-keep-it.md)
  — the round trips that ADR left per file were `open` and `release`; this
  one removes `read` and `write` on the files that still had them.
- Evidence: [`benchmarks/fuse-levers.md`](../../benchmarks/fuse-levers.md),
  [`plans/20260903-1000-fuse-levers.md`](../../plans/20260903-1000-fuse-levers.md)

## Context

After ADR 0014 a warm base blob costs the kernel's own floor: one `open`,
one `release`, and no `read` because the page cache keeps it. Everything
else still went through the daemon. A cold blob paid a `read`. An overlay
file paid a `read` on every open, because its pages were never kept, and
every `write` was a round trip that also committed a journal row: 244 µs
per 4 KiB write, 29 s to copy ten thousand files into a workspace, 216 µs
to reopen and read a file the job had just written.

Linux 6.9 added FUSE passthrough: the daemon answers `open` with a *backing
file*, and from then on the kernel performs the descriptor's reads, writes,
splices and `mmap` against that file directly. No request is made. The
kernel here (6.18) has it, `fuser` 0.18 exposes it, and one condition
applies: the ioctl that registers a backing file requires `CAP_SYS_ADMIN`
in the initial user namespace.

## Decision

**When the kernel offers passthrough and the daemon holds the capability,
every open of a base blob or an overlay file hands the kernel a backing
file.** The daemon asks for `FUSE_PASSTHROUGH` at `init` only if
`CapEff` carries `CAP_SYS_ADMIN`; otherwise nothing changes and a log line
says why.

- **A base blob in local mode** is inflated once into a memfd, registered,
  and the memfd is the backing file. The blob is dropped from the source's
  memory LRU (`SnapshotSource::forget_blob`) because the memfd now holds the
  bytes, so passthrough does not double the memory.
- **A base blob in server mode** uses its cache file as the backing file.
- **An overlay file** uses its content file, for reads and writes alike.

**One backing id per blob or content file, reused across opens.** The
kernel refuses two different backing files on one inode (`EBUSY`) and
refuses to mix passthrough and cached opens on one inode (`ETXTBSY`), so a
blob's registration is kept in a bounded LRU (256 MiB of memfds, 4 096
registrations) and every open handle holds a reference to the registration
it used; an id is closed only when the LRU and every handle have let go.
Overlay content registrations leave the LRU with the last handle on them,
so a deleted file's blocks are not pinned by a kernel reference the daemon
keeps for no one.

**The first refusal decides for the mount.** Because opens on one inode
cannot mix modes, a mount whose first registration fails (`EPERM` without
the capability) never tries again and behaves exactly as before; one whose
first succeeds keeps going.

**What the kernel writes behind the daemon, the daemon reads back.** A
passthrough writer's content file grows without a `write` request. While
the handle is open, `getattr` and `lookup` answer from an `fstat` of the
content file rather than the journal row, because the kernel invalidates
its cached size after each passthrough write and asks. At `release` the
size and mtime go into the journal row (`Overlay::refresh_content`), which
is what `git status`, the fsmonitor answer and the next commit read.

## Consequences

- **Deployment decides.** `setcap cap_sys_admin+ep gfs-fuse` on the daemon
  binary, or a container that grants the capability. That is a root-
  equivalent capability on a process that also holds the user's files, so
  it is a choice the operator makes, never a default the build makes. Every
  rebuild of the binary drops the file capability.
- **A copy-up while a reader holds the base blob's backing file makes the
  writer's `open` fail with `EBUSY`.** The reader's inode is bound to the
  memfd; the writer needs the content file. The daemon has already copied
  up when the kernel refuses, which is harmless, and the caller can retry
  once the reader closes. The same applies to a path deleted and re-created
  while a descriptor on the old content is open. Both are rare enough to
  document rather than design around.
- **The overlay quota is advisory under passthrough.** Bytes written by the
  kernel are counted at `release`, not admitted at `write`. A job can
  overshoot its quota by what it writes between an open and its close.
- **`gfs inspect` reports `reads`, `writes` and `written_bytes` only for
  what went through the daemon.** A new counter, `passthrough_opens`, says
  how much did not.
- **`FOPEN_KEEP_CACHE` and the listing cache from ADR 0014 stay.** A
  passthrough descriptor uses the backing file's page cache, not the FUSE
  inode's; directories are untouched.

## Alternatives considered

- **A memfd LRU shared across mounts of one clone.** Backing ids are per
  FUSE connection, so the registration cannot be shared; only the memfd
  could, and two mounts reading the same blob is not the common case.
- **Passthrough for base blobs only, keeping writes in the daemon.** The
  kernel forbids mixing modes on an inode, and a base blob's inode becomes
  an overlay file's inode at copy-up. Half measures do not compose.
- **Waiting for an unprivileged passthrough.** The kernel source carries a
  `TODO: relax CAP_SYS_ADMIN once backing files are visible to lsof`; when
  that lands the capability check in `init` becomes a no-op and nothing
  else here changes.
