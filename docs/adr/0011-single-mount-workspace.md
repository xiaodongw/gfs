# ADR 0011: A single-mount, self-contained workspace

- Status: Proposed
- Date: 2026-07-31
- Amends: [ADR 0009](0009-raw-git-over-a-projected-object-store.md) — the
  projection stays; its mountpoint and the state directory move inside the
  workspace. [ADR 0008](0008-one-host-many-mounts.md) — the host still serves
  many mounts; each workspace is now one mount instead of two.
- Evidence: [`spikes/reports/m05c-gitdir-through-fuse.md`](../../spikes/reports/m05c-gitdir-through-fuse.md)

## Context

A workspace today is three things in two places: the workspace mount
(`vscode/`), and a sibling state directory (`vscode.gfs/`) holding the real
git dir, the overlay, and a **second** FUSE mount projecting the object
database, wired to Git by a gitfile redirect and `objects/info/alternates`.
It works, but the seams show: `du` walks into the odb projection and reports
phantom gigabytes, teardown must sweep two mounts, tools that mishandle
gitfiles stumble, and a workspace cannot be picked up and moved because half
of it lives beside the folder rather than in it.

## Decision

**One mount, everything inside the folder, state shadowed under the
mountpoint.**

Creating a workspace:

1. create `vscode/` with a real `.git/` inside it; all GFS state — the
   overlay, the journal — lives under `.git/gfs/`, so the only reserved name
   in the presented namespace is one Git itself already forbids as a tracked
   path;
2. open directory handles to the real `.git` before mounting;
3. mount the workspace filesystem **over** `vscode/` — the on-disk contents
   are now shadowed, reachable only through the daemon's retained handles;
4. present the tree as before, plus `.git` **passed through** to the real
   directory — a normal directory to every tool, no gitfile;
5. inside `.git/objects/`, present `pack/` and `info/` as a **union**: the
   projected object-store files (ADR 0009's projection, relocated) merged
   with whatever Git writes locally — loose objects from commits, packs from
   fetches. Reads prefer local; writes always land in the real directory;
   pack names are checksums, so collisions do not occur. `alternates` is gone.

Consequences by construction: `du` no longer lies twice over (the projection
still advertises pack sizes, but there is exactly one place to measure);
unmounting leaves a folder that is a plain working tree with a fat `.git`;
copying that folder to another machine and running `gfs clone` there adopts
it — the adoption logic (`d3af3ae`) inspects one directory instead of two.

## What the spike settled

m05c measured the one cost this layout adds — every `.git` operation becomes
FUSE traffic — and found it **entirely a negative-lookup problem**: Git probes
its primary loose-object directories before packs and alternates (6,524
ENOENT lookups for one linux `read-tree`), and each probe is a FUSE round
trip. Data volume is irrelevant (a 10.2 MB index rewrite costs ~7 ms of
delta).

The fix is a kernel feature the filesystem merely has to use: **negative
dentries**. Replying to an absent-name lookup with a node-id-0 entry and a
TTL keeps repeated probes inside the kernel, across processes. With it, every
measured command lands within a few ms of local disk on the worst-case
repository. The kernel drops a negative dentry itself when a name is created
through the mount, and nothing can mutate the shadowed state behind the
daemon's back, so the caching is coherent by the same argument that makes the
layout self-contained.

**Negative-dentry caching on the object namespace is therefore a requirement
of this design, not an optimization.** Without it the layout is 3–6× slower
on object-heavy commands and loses on measurement.

## Constraints the implementation must honor

- **Recovery ordering.** Retained handles die with the daemon, and after a
  crash the state sits behind a stale mount. Recovery is: lazy-unmount, reopen
  the real `.git`, remount. The dead-mount sweep (`ca2c6f1`) already walks
  this path; the ordering becomes load-bearing.
- **Union semantics, minimally.** Only `objects/pack/` and `objects/info/`
  union projected content with local writes. Loose-object directories are
  purely local (the projection serves packs); the rest of `.git` is pure
  passthrough. Readdir merges, local wins on name collision, writes go local
  unconditionally.
- **Copy-while-mounted is a foot-gun.** `cp -r` on a live workspace walks the
  projection and hydrates the snapshot. The guide must say "unmount first",
  and a `gfs export` that does it safely is the durable answer.
- **Windows/macOS clients are unaffected**: this ADR is about the Linux FUSE
  layout; the WebDAV surface (ADR 0010) is orthogonal.

## Alternatives considered

- **Keep two mounts, move both inside the folder** — self-containment without
  passthrough `.git`. Rejected: keeps the gitfile redirect and alternates
  seams, and still needs the shadowing trick for the state directory, so it
  pays the novelty without collecting the compatibility wins.
- **Bind-mount the state dir elsewhere instead of retained handles** — a
  second mount by another name; teardown complexity returns.
- **Status quo** — works, but every seam listed in Context is a real support
  cost already paid once (the `du` confusion arrived within a day of real
  use).
