# ADR 0016: Let the kernel gather writes, and give it a new inode when the daemon moves a file under it

- Status: Accepted (implemented 2026-09-03, opt-in)
- Date: 2026-09-03
- Amends: [ADR 0014](0014-answer-on-the-fuse-thread-and-let-the-kernel-keep-it.md)
  — one more thing the kernel is allowed to keep — and
  [ADR 0011](0011-single-mount-workspace.md) — the passthrough `.git`
  tree acquires a rule about how the daemon may rewrite files in it.
- Evidence: [`benchmarks/fuse-levers.md`](../../benchmarks/fuse-levers.md)

## Context

Without passthrough (ADR 0015), which needs a capability most deployments
will not grant, every `write(2)` into the workspace is a FUSE request, and
for an overlay file that request also commits a journal row: 244 µs per
4 KiB write on this machine, 29 s to copy ten thousand files into a
workspace, 5 s for the `git add -A` that follows. A tool that writes in
small pieces — `cp`, `tar`, a compiler emitting an object file — pays per
piece.

The kernel has had a remedy since 3.15: `FUSE_WRITEBACK_CACHE`. Writes
land in the page cache and are sent later in `max_write`-sized requests
(16 MiB here), on `fsync`, on `close`, or under memory pressure. It comes
with one rule, stated in `fs/fuse/inode.c`: with the flag on, the kernel
trusts its own cached size, mtime and ctime for every regular file and
ignores what `getattr` says about them. A regular file whose bytes change
*without* going through the kernel therefore keeps its old size in the
kernel's eyes for as long as the kernel holds the inode — `inval_inode`
drops pages and attributes, not the size.

This mount changes regular files behind the kernel in exactly one
situation: a re-pin rewrites the `.git` seed (`HEAD`, `index`,
`packed-refs`, `config`, `gfs.json`) and, for the merged tree, the base
under every path. And `mount.json` under `.git/gfs/` is rewritten whenever
the lease or the pin changes.

The flag has a second consequence the kernel does not advertise as
loudly: a `write(2)` into the page cache always succeeds. The daemon's
answer to the write — including `EDQUOT` from the overlay quota and `EIO`
from a lost server — arrives at writeback and is reported by `fsync` or
`close`. A program that ignores `close`'s result loses those bytes without
being told; a loop that writes until the first error never ends (the quota
test did exactly that).

## Decision

**Ask for `FUSE_WRITEBACK_CACHE` at `init` when the mount was created with
`--writeback-cache`, and not otherwise.** The write handler is unchanged;
it receives fewer, larger requests. Off by default because the quota
contract (ADR 0006, PLAN.md M3.2: refuse the new write, keep the old ones)
is a promise about `write(2)`, and this flag moves the refusal to `close`.
The operator who turns it on is choosing throughput over that promise for
the tools they run, most of which check `close`.

**Every inode record carries a generation, and a re-pin bumps it for every
regular file the kernel may hold.** The generation goes into every `lookup`,
`create` and `readdirplus` reply. On the next lookup after the re-pin's
`inval_entry`, the kernel compares generations, marks the old inode bad,
drops it from its cache and instantiates a new one with the size the daemon
reports. Directories and symlinks keep their generation, so a shell whose
working directory is inside the workspace survives a `gfs switch`.

**`persist()` does the same for `mount.json`** through
`Gfs::rewrote_behind_kernel`, which bumps the record if the kernel has one
and names the dentry to invalidate.

## Consequences

- **With the flag on, a refused write surfaces at `close` or `fsync`.**
  `cp`, `tar`, compilers and Git check; a shell redirection does not. An
  unmount while a descriptor is still open can also lose bytes the writer
  saw acknowledged, because they were acknowledged by the page cache.
- **A descriptor held open across a re-pin now reads `EIO`** instead of a
  mixture of old and new bytes. That is the kernel's behaviour for a bad
  inode, and the honest one: the file the descriptor named is not the file
  the path names any more.
- **The rule for the passthrough `.git` tree is now explicit.** The daemon
  may write a regular file there behind the kernel only at re-pin or
  through a path that calls `rewrote_behind_kernel`. The overlay journal
  under `.git/gfs/` is exempt because it is daemon-private: nothing reads
  it through the mount, and a tool that does will see the size the kernel
  cached at its first `stat`.
- **mtime and ctime flushed by the kernel** arrive as `setattr` calls with
  `FATTR_MTIME | FATTR_CTIME`. The overlay stores the mtime and assigns its
  own ctime; after the inode is evicted Git sees a ctime it did not record
  and rehashes the file once.
- **`git status` and the fsmonitor answer are unaffected.** They read the
  journal, which is written when the requests arrive; a `close` flushes
  before it returns, so a file is in the journal before the tool that wrote
  it can ask Git about it.
- **The two levers do not stack.** The kernel refuses `FUSE_PASSTHROUGH`
  together with `FUSE_WRITEBACK_CACHE` and, asked for both, grants neither
  (found by measurement: a mount with both flags served zero passthrough
  opens). A mount that has passthrough drops the writeback request and
  logs it. Passthrough is the better of the two for writes anyway: the same
  52 µs per 4 KiB write, and the refusal semantics of `write(2)` are the
  content file's, not the page cache's.

## Alternatives considered

- **On by default.** The first version was, until the quota test looped
  forever on a `write(2)` that could no longer fail. The generation
  mechanism handles the size hazard; nothing handles the error-timing one,
  so the default stays off and the flag says what it trades.
- **Invalidate harder at re-pin.** `inval_inode` with an offset and length
  drops pages; nothing in the notification protocol resets a size the
  kernel owns. Only a generation change does.
- **Bump generations for directories too.** Correct but hostile: every
  open directory handle and every working directory inside the workspace
  would go bad on a re-pin, for nothing — a directory has no size the
  kernel caches.
