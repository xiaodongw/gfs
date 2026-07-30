# Projection gaps: eviction, degrade shims, push, attribution

## Summary

ADR 0009's build (M9.1–M9.6) landed with four gaps recorded in its amendment
and in `plans/20260729-1501-git-projection-spike.md`. This session closes them
in order of self-containment:

1. **Residency-budget eviction** in the odb block store. The counters exist;
   the policy — bytes held, evict-and-refetch, never refuse, scan-resistant —
   does not.
2. **`grep`/`find` degrade shims.** ADR 0009 narrows ADR 0007's question to "a
   degrade rule rather than a refusal": the tools must work, and the budget is
   the enforcement, but the shim should say the cheap route first.
3. **`git push` to the gateway** (smart-HTTP receive-pack), so commits stop
   leaving through `CommitChanges`.
4. **Per-job odb attribution** — flagged in the amendment as needing a
   different layer than the shared projection; expect a decision, not
   necessarily code.

Plus one investigation: the non-reproducible `mutations` flake under
full-suite parallelism.

## Plan

### Phase 1 — residency eviction (odb block store)

- `BlockStore` gains a residency limit (bytes held on local disk; 0 = off,
  matching ADR 0009's opt-in row in the two-budgets table).
- Policy is **SLRU** (probation/protected), because the ADR requires scan
  resistance: a first-touch block enters probation; a re-read while resident
  promotes to protected (capped at half the limit, demoting back to
  probation). Victims come from probation LRU first, then protected LRU. A
  one-pass streaming `blame` never promotes anything, so it can only flush
  probation — the build's re-read blocks survive arbitrarily large scans.
- Eviction clears the presence bit and punches a hole
  (`fallocate(FALLoc_FL_PUNCH_HOLE)`) so the disk is actually returned.
- Blocks of an in-flight read are **pinned**: presence-marking and eviction
  share one lock, and a pinned block cannot be evicted between being fetched
  and being served — the "bitmap that lied" hazard applied to eviction.
- New counters: resident bytes, evicted blocks, refetched blocks/bytes
  (distinct from first-time fetches via an ever-fetched bitmap). The
  refetch/unique ratio is the thrash signal the ADR says residency mode must
  report. Stats surface in `MountReport` for `gfs status`.
- Config: `--odb-residency-budget` on `gfs-fuse` (env
  `GFS_ODB_RESIDENCY_BUDGET`), default 0.

### Phase 2 — grep/find degrade shims

Mirror the git shim's structure: transparent outside a GFS workspace; inside
one, advise on stderr (naming `gfs rg` / `git ls-files` and the measured
costs) and run the real tool. Never substitute output.

### Phase 3 — receive-pack on the gateway

Smart-HTTP `git-receive-pack` next to the existing upload-pack route, so the
workspace's real `.git` can push its local commits. Ref updates restricted to
the mount's work branch; `CommitChanges` stays for API callers.

### Phase 4 — per-job odb attribution

Design note first: the projection is shared per repository, so per-job
identity does not exist at the block store. Candidate: attribute at the
workspace boundary (the `.git` alternates consumer) rather than the store.

### Phase 5 — mutations flake

Reproduce under contention (`--test-threads` high, repeated runs); if it is
FUSE mount contention, bound or serialize mount setup in the test harness.

## Decisions

**Eviction is SLRU, not LRU, and pins are what make it sound.** ADR 0009's
scan-resistance requirement rules plain LRU out: a one-pass `blame` would
flush a build's working set. Probation/protected (protected capped at half the
limit) means single-touch blocks can only flush each other. The subtle hazard
was eviction racing an in-flight read — a punched hole reads as zeros, the
"bitmap that lied" with a new face — so a read pins its block range under the
same lock that marks presence and chooses victims; pinned blocks are never
victims, and a fully pinned store simply stays over the limit (the same answer
the blob cache gives). Disk is returned with `fallocate(FALLOC_FL_PUNCH_HOLE)`
— the one new `#[allow(unsafe_code)]` site, following the `attr.rs` precedent.

**The scan shim advises; it never refuses and never substitutes.** Unlike the
git shim's five refused maintenance commands, `grep -r`/`find`/`rg` *work* —
the hydration budget is the enforcement, the note names the cheap route
(`gfs rg`, `git ls-files`). Non-recursive `grep` is not nagged. One binary
dispatching on argv[0], linked by `gfs install-shim`; absence of the binary
degrades to stock behaviour, not an error.

**Push confinement is `receive.hideRefs` order plus a pre-spawn parse.** Git
evaluates hideRefs back to front, so `refs/` then `!refs/gfs/work/<subject>`
hides everything except the caller's own subtree — and `-c` values are read
after repository config, so a repository cannot append a wider negation. The
gateway also parses the command section (pkt-lines up to the first flush; the
pack after it is raw bytes, which is why `decode` alone would misread the
body) and refuses out-of-tree ref updates by name. Chosen over GIT_NAMESPACE —
which would have moved the work-ref layout to `refs/namespaces/` and rippled
through every work_ref consumer — and over rewriting refnames on the wire,
which is the protocol surgery this gateway exists to avoid.

**The client learns its push target from `CreateMountResponse`.** The server
folds subject ids into ref names; the client cannot re-derive that, so the
work-ref root travels in the mount grant (additive field 9, golden schema
updated). The seeded config maps `refs/heads/*` into it and authenticates via
a credential helper reading `GFS_TOKEN` at push time — no token on disk.

**Attribution is per-view, not per-pid.** Each mount gets its own `OdbView` —
a second small FUSE mount over the shared `BlockStore` — and the workspace's
alternates points there. Which-mountpoint-was-read is unforgeable job
identity; pid lookups stay rejected. `BlockStore::read` now returns its
`ReadCost` so the view can accumulate it; blocks, residency, and eviction stay
shared.

## Details

- `--odb-residency-budget` / `GFS_ODB_RESIDENCY_BUDGET` on `gfs-fuse`;
  0 (default) = unbounded. Per repository per host, like the store itself.
- `gfs status` now shows an `odb` line (repo-wide: fetched, resident, evicted,
  refetched) and a `this job:` line (the view's share).
- The kernel page cache sits above the odb view (`FOPEN_KEEP_CACHE`), so an
  evicted block re-read by the same process may be served without a re-fetch —
  correct bytes, invisible eviction. The eviction test asserts refetch
  accounting through the store API, below the page cache.
- Push namespace: `git push origin <branch>` lands at
  `refs/gfs/work/<subject>/<branch>`. Deletes and force-pushes of one's own
  work refs are allowed (mounts pin through lease anchors, not work refs).
  `receive.autogc=false` — a post-receive gc could repack away files live
  projections reference. Pushed packs only *add* files, which the static
  manifest model tolerates; a projection mounted before the push simply does
  not list the new pack.
- Repin over pushed-but-unpinned local commits still refuses (`local_head`
  guard checks the pinned commit, not server state). Fine for now: the guard
  errs toward refusing; `gfs switch` to the pushed work branch is the path.
- The receive-pack advertisement scanner allows exactly the caller's subtree
  (`AdvertisementScanner::allowing`); every other appearance of `refs/gfs/`
  still aborts the stream.
- Mutations flake: not reproduced in 8 consecutive full-suite runs; no
  timing-sensitive code in `mutations.rs` (no sleeps, deadlines, or clock
  assertions). Left as monitored; nothing changed.
- Each workspace now costs one extra FUSE session (its odb view). Worth
  remembering if mount-per-host counts ever matter.
