# Single-Mount Self-Contained Workspace

## Summary

Collapse the workspace's two mounts and sibling state directory into one
self-contained folder: mount the workspace filesystem *over* a directory that
physically contains the real `.git` (with all GFS state under `.git/gfs/`),
reach the shadowed state through directory handles retained from before the
mount, pass `.git` through as a normal directory, and union the ADR 0009
object-store projection into `.git/objects/{pack,info}` in place of the
`alternates` redirect. Proposed by the user; feasibility and performance
validated by spike m05c. Design recorded in ADR 0011 (Proposed).

The spike's decisive finding: `.git` through FUSE is at parity with local
disk **only** with kernel negative-dentry caching on absent-object lookups —
without it, object-heavy commands are 3–6× slower. Negative-dentry caching is
a requirement of the design.

## Plan

_To be filled in when work starts on this issue._

Expected shape (from ADR 0011): mount-over-state lifecycle in gfs-fuse
(create, retained handles, recovery ordering: lazy-unmount → reopen →
remount), `.git` passthrough with negative-dentry replies on the object
namespace, union readdir for `objects/pack` and `objects/info`, adoption of
copied folders in `gfs clone`, `gfs export` for safe copying, migration of
the dead-mount sweep, TESTING.md and manual-test.md updates.

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
- Union scope is minimal: only `objects/pack/` and `objects/info/` merge
  projected and local content; loose dirs are purely local; the rest of
  `.git` is pure passthrough.

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
