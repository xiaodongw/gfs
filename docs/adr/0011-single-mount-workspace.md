# ADR 0011: A single-mount, self-contained workspace

- Status: Accepted (implemented 2026-07-31)
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
5. present ADR 0009's object-store projection at **`.git/gfs/objects/`**, and
   point `objects/info/alternates` at it with the **relative** path
   `../gfs/objects` — resolved by Git against the objects directory, so the
   pointer travels with the folder. `objects/` itself stays purely local:
   loose objects from commits and packs from fetches land there untouched.

The mount therefore has exactly two subtree behaviors — pure passthrough and
pure projection — and no merged namespace anywhere. An earlier draft of this
ADR removed `alternates` and unioned the projection into `objects/pack/`;
that union was the largest piece of novel FUSE logic in the design and its
riskiest correctness surface (readdir merging under concurrent mutation,
`tmp_pack_*` renames, `.idx`/`.pack` visibility ordering, repack refresh
interleaved with local entries). Keeping the one-line alternates file — a
stock Git mechanism this system already uses, invisible to every Git-speaking
tool — deletes all of it for no functional loss.

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
- **The alternates path must stay relative.** `../gfs/objects` is what makes
  a copied folder work on another machine; an absolute path would silently
  re-introduce the location dependence this ADR exists to remove. Adoption
  (`gfs clone` over a copied folder) should verify and repair it.
- **Copy-while-mounted is a foot-gun.** `cp -r` on a live workspace walks the
  projection and hydrates the snapshot. The guide must say "unmount first",
  and a `gfs export` that does it safely is the durable answer.
- **Windows/macOS clients are unaffected**: this ADR is about the Linux FUSE
  layout; the WebDAV surface (ADR 0010) is orthogonal.

## What implementation added (2026-07-31)

Two facts surfaced while building this that refine the design without
changing it:

- **The daemon must never do its own I/O through its own mount, and two
  overlay details conspired to make it.** SQLite canonicalizes database
  paths — an overlay journal opened through the retained `/proc/self/fd/<n>`
  handle *after* mounting resolves back to the on-disk name and opens through
  the mount itself. So the journal's connection is opened at the on-disk path
  in the one window it still resolves to the real directory — before the
  mount — and lives for the mount's life; a re-pin clears and re-points it in
  one SQLite transaction (`Overlay::rebind`) instead of opening a fresh
  per-epoch directory. One overlay directory, `.git/gfs/overlay/`, no epochs.
  The content store is the opposite case: it opens files per operation, so
  its root must be the handle path (plain opens do not canonicalize) — with
  the on-disk root, a copy-up's directory fsync arrives back at the daemon as
  `FUSE_FSYNCDIR` while the copy-up still holds the overlay lock the handler
  needs, and the mount deadlocks against itself. And even with both fixed,
  SQLite itself fsyncs the journal's *directory* by canonicalized path on
  some commits — through the mount, mid-commit, lock held — so the
  `fsyncdir` handler routes by subtree: a `.git` directory handle syncs the
  real directory and never touches `Overlay::sync`. The general law all
  three instances obey: nothing the daemon holds a lock around may reach its
  own mount, and anything that canonicalizes paths will find its way there.
- **The control socket lives in the runtime directory, not the folder.** A
  Unix socket cannot be connected to through a FUSE passthrough — `connect(2)`
  needs the inode the daemon bound, and the passthrough can only present a
  look-alike. The socket is a runtime artifact, not state, so it moves to
  `$XDG_RUNTIME_DIR/gfs/ws-<hash-of-workspace-path>.sock`; `.git/gfs.json`
  records the path, which is how tools discover it by walking up — mounted or
  not, the folder itself still carries everything durable.

## Alternatives considered

- **Union the projection into `objects/pack/`, no alternates** — the earlier
  draft. Rejected as described in the Decision: the union is the design's
  largest and riskiest new mechanism, bought only to delete a one-line file
  that no tool ever sees. A raw scanner that reads `.git/objects/pack`
  without understanding alternates would miss projected packs; git, libgit2,
  and everything built on them are not that scanner.
- **Keep two mounts, move both inside the folder** — self-containment without
  passthrough `.git`. Rejected: keeps the gitfile redirect seam and still
  needs the shadowing trick for the state directory, so it pays the novelty
  without collecting the compatibility wins.
- **Bind-mount the state dir elsewhere instead of retained handles** — a
  second mount by another name; teardown complexity returns.
- **Status quo** — works, but every seam listed in Context is a real support
  cost already paid once (the `du` confusion arrived within a day of real
  use).
