# ADR 0013: Local mode — a workspace over a clone on this machine

- Status: Accepted (implemented 2026-09-02)
- Date: 2026-09-02
- Amends: [ADR 0009](0009-raw-git-over-a-projected-object-store.md) — the
  projection is one way to put an object store behind the workspace, not the
  only one. [ADR 0007](0007-tool-surface-in-the-agent-image.md) — the shims
  are a property of the remote deployment, not of the mount.
- Evidence: [`benchmarks/local-mode.md`](../../benchmarks/local-mode.md)

## Context

GFS was built for a job in a container reading a repository it does not have.
A different reader turned up: a developer who already has a full clone of a
monorepo and wants one working tree per change. Their tool is `git worktree
add`, and on a monorepo it copies the whole tree every time — measured on this
machine at 2.6 s and 302 MiB for vscode, 1.3 s and 75 MiB for django, and
worse on a real monorepo. Five changes in flight is five copies.

The mount already presents a commit lazily. What stood between it and this
reader was that every question the filesystem asked went to `gfs-server`
through one concrete client type, and the answers it needed — tree, blobs,
index, refs, history, search — all exist a few centimetres away in the clone's
own `.git/objects`.

## Decision

**Put a trait between the filesystem and its snapshot source, and implement it
over a local clone.** `gfs mount --local <clone>` serves a workspace with no
server, no lease, and nothing on `PATH`.

`SnapshotSource` is the read surface the mount actually uses: entry, directory
and tree pages, blob bytes, the per-commit index, commit metadata, log,
resolve, refs, diff, blame, search, and the lease pair. `SnapshotClient` (gRPC
and HTTP) is one implementation. `LocalSource` is the other, over `gfs-git`'s
`GitRepository` — the same libgit2 boundary the server reads through — with
every call dispatched to `spawn_blocking` under the handle pool's admission
control. Three capability hooks let a source decline rather than fake:
`leased()`, `serves_blobs_in_memory()`, and `capability_for_persistence()`.

In local mode:

- **The clone is the object store.** `objects/info/alternates` names the
  clone's `objects` directory by absolute path, which is what `git worktree`
  does. There is no projection at `.git/gfs/objects`, no block cache, and no
  residency budget. Stock Git reads the clone's packs directly, and a commit
  in the workspace writes loose objects into the workspace's own `.git`.
- **The pin is an anchor ref in the clone.** `refs/gfs/mounts/<id>` holds the
  commit reachable so `git gc` in the clone cannot prune what a workspace is
  standing on. It is the same reserved-namespace ref the server writes, and it
  is deleted at unmount. This is the one thing local mode writes into the
  clone.
- **Blobs are served from memory.** The verified on-disk cache exists to make
  a network fetch happen once. Locally it would write a second copy of every
  file read onto the disk the pack is already on. A read inflates the blob,
  holds it for the life of the descriptor, and a bounded per-clone LRU keeps
  the hot set. The hydration budget is off: nothing crosses a network.
- **Search is a parallel scan.** The server's trigram index costs seconds to
  build per snapshot and lives in the server's store. Local mode walks the
  tree and runs the overlay's own scanner over slices of the sorted path list
  on as many threads as the pool has handles, reading straight from the pack.
  No index, so no `SNAPSHOT_BUILDING`; coverage and truncation are reported
  exactly as the server reports them.
- **The clone is `origin`.** `git fetch` and `git push origin HEAD:<branch>`
  move work between the workspace and the clone over the filesystem. Git
  refuses a push onto the clone's checked-out branch, which is the right
  refusal.

What does not change: the listing cache and walk detector, the overlay and
its journal, the fsmonitor hook, the LFS filter driver, `gfs status`, the
control socket, and the CLI. The heartbeat is simply not run for an unleased
source, and health reads `Healthy`.

## What goes and what stays, on the tool surface

ADR 0007's shims route *cost* away from the server. With the object store on
local disk there is no cost to route: `git gc`, `repack`, `blame` are as cheap
as in any clone, and `rg` over the mount pays FUSE and inflate but no network
and no cache write. So local mode installs no `git`, `grep`, `find`, or `rg`
shim. The two hooks the daemon installs are not shims and stay:

- `gfs-fsmonitor`, because `git status` over 18 000 files through FUSE is still
  18 000 lookups without it, and the journal still knows what changed;
- `gfs-lfs-filter`, because a host with git-lfs installed has a global
  `filter.lfs.process` that would otherwise try to download from the clone's
  `origin`, and the driver's truthful pointer handling is what keeps `git add`
  from committing garbage.

## Consequences

- **A workspace is bound to its clone.** The absolute `alternates` path is the
  point: a folder copied to another machine cannot outlive the clone it
  borrows from. ADR 0011's copied-folder story applies to remote workspaces
  only.
- **LFS pointers are not expanded.** The clone has no LFS store the daemon
  knows about. The tree shows pointer files, as `GIT_LFS_SKIP_SMUDGE=1` would,
  and the filter driver keeps them honest. A local LFS store is a separate
  decision.
- **`git gc` in the clone while a workspace is mounted is safe** (the anchor)
  but a crashed daemon leaves its anchor behind. `git for-each-ref
  refs/gfs/mounts/` shows them; the next mount does not sweep them.
- **Two sources, one filesystem.** Every future capability of the mount has to
  be answered by both implementations or declined by one through a hook, which
  is the discipline the trait buys.

## Alternatives considered

- **A local `gfs-server` beside the clone.** Works today with no code, and
  costs a second process, an import, a lease, a capability, and a blob cache
  copy of every file read — everything the developer does not need. The
  benchmark records what that costs.
- **Keep the disk blob cache in local mode.** Simpler, but every file a build
  opens is then written twice to the same disk, which is the worktree cost
  under another name.
- **Build the trigram index locally.** Faster queries after a cold build of
  seconds to a minute per commit, in a store that would have to live
  somewhere the clone is. A scan reads the pack once per query and needs no
  home; if query latency on a real monorepo turns out to matter, the index
  build in `gfs-service` can move down into `gfs-search` and both sources can
  share it.
- **Make the workspace's `.git/objects` a symlink into the clone**, so commits
  land there directly. Rejected: it makes the workspace's writes the clone's
  problem and the `alternates` line already gives Git the read side for free;
  `git push origin` is one command and leaves the clone's refs to the clone.
