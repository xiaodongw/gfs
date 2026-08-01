# Single-Mount Self-Contained Workspace

## Summary

Collapse the workspace's two mounts and sibling state directory into one
self-contained folder: mount the workspace filesystem *over* a directory that
physically contains the real `.git` (with all GFS state under `.git/gfs/`),
reach the shadowed state through directory handles retained from before the
mount, pass `.git` through as a normal directory, and present the ADR 0009
object-store projection at `.git/gfs/objects/`, pointed to by a **relative**
`objects/info/alternates` (`../gfs/objects`) so the pointer travels with the
folder. Proposed by the user; feasibility and performance validated by spike
m05c. Design recorded in ADR 0011 (Proposed).

The spike's decisive finding: `.git` through FUSE is at parity with local
disk **only** with kernel negative-dentry caching on absent-object lookups —
without it, object-heavy commands are 3–6× slower. Negative-dentry caching is
a requirement of the design.

## Plan

**Implemented 2026-07-31** (ADR 0011 → Accepted). What landed:

- `crates/gfs-mount/src/passthrough.rs` (new): retained `GitDirHandle`
  (`/proc/self/fd/<n>`), `GitPassthrough` serving `.git/**` writably
  (lockfile `O_EXCL`, rename, unlink, hard links, chmod/truncate/utimens)
  and the projection tree at `.git/gfs/objects/**` from the shared
  `BlockStore`, with per-workspace attribution counters.
- `fs.rs`: subtree routing (passthrough / projection / merged view),
  negative dentries with a 60 s TTL confined to the object namespace
  (`FsConfig::object_negative_ttl`), short `git_ttl` for passthrough attrs.
- `mount.rs`: single-mount lifecycle — sweep (lazy-unmount first), open
  retained handle, resolve, open the one overlay pre-mount, seed (relative
  alternates, no `core.worktree`), mount over; legacy `<ws>.gfs` migration;
  refused mounts unwind a freshly created `.git`.
- `odb.rs`: FUSE surfaces deleted (`OdbFs`, `OdbView`, projection mount);
  the store remains, shared per repository.
- Overlay: one directory, no epochs — `Overlay::rebind` clears and re-binds
  in a single SQLite transaction on repin (SQLite resolves symlinks, so the
  connection must be opened pre-mount and kept).
- Control socket in `$XDG_RUNTIME_DIR/gfs/ws-<hash>.sock` (a socket cannot
  be connected to through a passthrough); `.git/gfs.json` records it; CLI
  discovery walks up to `.git/gfs`, with legacy shapes still recognized.
- Shims (`gfs-git-shim`, `gfs-scan-shim`, `gfs-fsmonitor`) accept the real
  `.git` directory shape; `STATE_FORMAT_VERSION` = 2; scripts and
  manual-test.md updated; `gfs export` remains future work.

## Decisions

- Overmount-and-retain-handles over bind-mounts or a sibling directory: the
  folder is self-contained (copyable, adoptable) and the presented namespace
  reserves only `.git`, which Git already forbids as a tracked name.
- All GFS state under `.git/gfs/` rather than a sibling `overlay/`: no name
  the repository could contain is ever special.
- Negative-dentry caching (node-id-0 entry with TTL) is mandatory on the
  object namespace: m05c measured 6,524 ENOENT probes for one linux
  `read-tree`; with the caching, warm commands land within a few ms of local
  disk (worst measured: `status` +6%, `commit` +10 ms).
- Attribute TTL is not a lever for this problem: 1 s vs 60 s made no
  difference in any arm.
- No union at all (user's follow-up, 2026-07-31): keep `objects/info/alternates`
  with the relative path `../gfs/objects` instead of merging the projection
  into `objects/pack/`. The union was the design's largest new FUSE mechanism
  and riskiest correctness surface, bought only to delete a one-line file no
  Git-speaking tool ever sees. The mount now has exactly two subtree
  behaviors: pure passthrough and pure projection.

## Details

- Spike instrument: `spikes/git-projection` gained `--rw` (writable
  passthrough: create/write/setattr/mkdir/unlink/rmdir/rename/link/fsync,
  real modes and mtimes) and `--negative-ttl`; driver
  `measure-gitdir.sh`; results in `spikes/reports/m05c-gitdir-through-fuse.md`
  and `~/gfs-corpus/projection/{django,linux}/out/gitdir-*.json`.
- The first run in a freshly `cp -a`-staged arm rewrites the whole index
  (new inodes → stale stat data). GFS proper ships `core.checkStat=minimal`
  indexes, so this is the instrument's artifact, not the design's; warm
  numbers are the honest ones.
- Copy-while-mounted hydrates the snapshot through the projection — the
  operational answer is "unmount first" plus a future `gfs export`.
