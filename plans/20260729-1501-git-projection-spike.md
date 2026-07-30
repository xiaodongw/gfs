# Raw Git over a projected object store — feasibility spike

## Summary

GFS currently synthesizes a fake, object-free `.git` (ADR 0005) and re-implements
Git's read commands as server-side tools: `gfs log`, `gfs find`, `gfs show`,
`gfs diff`, `gfs blame`. ADR 0005 has been amended twice in four days, each time
moving one more question to the server, which is the shape of a treadmill rather
than a boundary.

This spike measures the alternative: **keep the worktree lazily projected from a
pinned commit, and give the workspace a real `.git` whose object database is a
read-only projection of the gateway's**. Raw `git` then answers every history
question, and the invented subcommands can be deleted.

The design under test has three parts, all of which are stock Git mechanisms
rather than new invention:

1. **`git worktree`'s state split.** A linked worktree's `.git` is a *file*
   pointing at a small per-worktree directory (`HEAD`, `index`, `logs/`), with
   `commondir` naming the shared `objects/`. Per-worktree state is small and
   writable; the object store is shared and read-only.
2. **`objects/info/alternates`.** Git's own "my object database is over there,
   shared and read-only" — what `git clone --shared` and worktrees use. The
   agent's `.git` is plain local disk; only `objects/` is projected.
3. **`core.checkStat=minimal` + `core.fsmonitor` + `core.untrackedCache`.** The
   three settings that decide whether `git status` is O(changed) or O(tree).

ADR 0005 rejected a client-side partial clone because `git status` stats every
index entry — 94 850 first-time FUSE lookups on the kernel. That measurement was
taken with Git's defaults, and `spikes/reports/m05-git-surface.md` says so
explicitly: "`core.untrackedCache` and `core.fsmonitor` can reduce the sweep
substantially and were not evaluated; neither is available through a FUSE mount
without further work, which is why they do not change the decision." The second
clause is the load-bearing one and this spike tests it, because `core.fsmonitor`
is a hook program and GFS's overlay journal is exactly the modified-path set it
asks for.

Scratch validation on a two-file repository already showed the semantics hold:
`git log`/`diff`/`commit`/`checkout -b` all work against a `chmod -R a-w` shared
object store, writing only local loose objects. What is unmeasured is **cost at
monorepo scale**, which is the whole question.

## Plan

Done. `spikes/git-projection` is a counting read-only passthrough FUSE
filesystem plus a driver script, and
[`spikes/reports/m05b-git-projection.md`](../spikes/reports/m05b-git-projection.md)
is the report.

1. **Instrument** — a passthrough FUSE fs that forwards to a local directory and
   counts lookups, opens, reads, bytes and readdirs bucketed by path class
   (worktree / `*.pack` / `*.idx` / loose object / git metadata). Counts, not
   latency: one lookup is one snapshot-API round trip in a real mount, and a byte
   under `*.pack` is a byte a gateway must ship.
2. **Stage** — the gateway builds the checkout, normalizes every mtime to
   `snapshot_time`, and refreshes an index against it. That index is what ships.
3. **Measure** in five phases: semantics; `git status` under three
   configurations; the history commands cold, warm, and with a commit-graph; the
   history commands again with shared metadata pinned per node; and the
   maintenance footguns.
4. **Corpora** — django (7 078 files) and linux (94 851 files, the same tip
   ADR 0005's 94 850-entry measurement used).

## Decisions

**Measure Git's demand, not GFS's latency.** The probe is a passthrough over
local disk rather than a client of the real gateway. A real end-to-end mount
would have required implementing pack projection first — the feature, not the
spike — and would have measured this machine's disk and gRPC stack alongside the
thing in question. Counts and bytes transfer to any implementation; wall times
from a local passthrough are a floor and the report says so.

**One mount per command for the cold numbers.** Within one mount the kernel
page-caches pack pages under `FOPEN_KEEP_CACHE`, so a later command can read zero
bytes purely because an earlier one paid. The first version of phase 3 measured
exactly that and reported `diff --stat HEAD~5 HEAD` as free. Both are now
reported: cold (fresh mount per command) and warm (one session).

**Split `*.pack` from `*.idx` in the accounting.** Added after the first run
showed large "pack" numbers, because the two have opposite design implications:
pack data scales with what a command needed, while the lookup structures are
identical for every mount and can be pinned once per node. The split is what
turned a vague cost into phase 5's recommendation.

**Skip the repack of an already-single-pack mirror.** `clone --mirror` yields one
server-generated pack; repacking linux's 8.5 GiB to reach the same shape costs
tens of minutes.

## Details

### The build (same day, branch `git-projection`)

M9.1–M9.6 were implemented after the spike, in dependency order, each phase
compiled and tested before the next:

* **M9.1** — retention policy in every mirror's own config: `gc.pruneExpire`
  derived from ADR 0006's `prune_delay`, `gc.auto=0`, `maintenance.auto=false`,
  `repack.cruftPacks=true`.
* **M9.3** — three HTTP routes (`/odb` manifest, `/odb/{path}` range reads under
  an allowlist grammar that *is* the security boundary, `/index?commit=`), a
  git-index-v2 writer in `gfs-git` whose entries record `snapshot_time` stat
  data, and `commit-graph write --split --changed-paths` at ingest. The
  round-trip test materializes a tree at `snapshot_time` and asserts stock
  `git status` reads the shipped index clean.
* **M9.2** — `gfs-mount/src/odb.rs`: 64 KiB block store over sparse local files
  (presence bitmap in memory only — a bitmap that lied would serve zeros as
  pack bytes), absent blocks fetched one HTTP range per contiguous run, and a
  small read-only FUSE fs (`OdbFs`), one per repository per host, shared via
  the same `Weak` registry as the blob cache.
* **M9.4** — the workspace `.git` became a synthesized *file* naming
  `<state>/git`, seeded with HEAD, the pinned branch ref, the required config,
  `objects/info/alternates` into the projection, and the shipped index. The
  fsmonitor hook (`gfs-fsmonitor` binary → control socket →
  `Mount::fsmonitor_changes`) answers cumulatively from the overlay status with
  a `gfs:<generation>` token; a token from another generation forces one full
  rescan. Repin refuses over local commits (`gitdir::local_head`).
* **M9.6** — the shim shrank from 842 lines of frozen grammar to ~130 of
  route/fail/pass, transparent outside a GFS workspace.
* **M9.5** — `gfs find`/`gfs log` CLI and the `FindPaths` RPC deleted end to
  end; `docs/agent-search.md` rewritten.

Two deltas from the ADR, recorded in its 2026-07-29 amendment: the `Log` RPC
survives as `gfs show`'s one-commit header fetch, and `blame` advises on stderr
instead of routing, because `gfs blame`'s output format is not `git blame`'s
and silently substituting it is the `ls-files` lie with a new face.

Known gaps, deliberately left: the residency-budget *eviction* policy is not
implemented (the block store counts what it needs to decide later; ADR 0009's
accounting section stands); odb traffic is counted per repository, not per job
(the projection is shared, so job attribution needs a different layer); the
direct-`Gfs` test harness still builds ADR 0005's six-entry surface for FUSE
fundamentals tests, marked legacy. One flake observed once under full-suite
parallelism (`mutations`, not reproducible alone or on rerun) — likely FUSE
mount contention across ~20 concurrent mounts.

Two harness bugs are worth remembering, because both produced plausible wrong
answers rather than errors:

- **`lower/objects` was a symlink to the mirror.** Git followed it to the
  absolute path underneath and read every object off local disk, so the first run
  reported *zero* pack traffic for commands that certainly read packs. Fixed with
  a hardlinked copy. A symlink in a projection is an escape hatch out of it.
- **`git ls-files | grep | head -1` aborted the script under `pipefail`**, because
  closing the pipe early sends `SIGPIPE` to `ls-files`. Every stage now reads to
  EOF (`sed -n 1p`).

Findings, in brief; numbers and caveats in the report.

- `git status` on 94 851 files: 108 445 lookups and 1 615 MiB read under Git's
  defaults, **170 lookups and 49 ms** with `core.checkStat=minimal`,
  `core.trustctime=false`, `core.fsmonitor` and `core.untrackedCache`. ADR 0005's
  central measurement does not survive the configuration its own spike report
  flagged as unevaluated.
- The defaults are worse than ADR 0005 derived: not a metadata sweep but a full
  content re-hash, because a shipped index's `dev`/`ino` cannot match another
  filesystem. `checkStat=minimal` fixes it, and DESIGN.md section 8.2's
  deterministic `snapshot_time` is what makes one shipped index valid on every
  host.
- The dominant history cost is the `.idx` and the commit-graph, not object data.
  Both are immutable and identical per repository, so pinning them per node
  (632 MiB for linux) takes their traffic to zero.
- `gfs find` and `gfs log` are redundant: `ls-files` costs zero projection
  traffic, `log` costs 0–16 MiB.
- `gfs show` and `gfs blame` are **not** redundant: 91.5 MiB and 196 MiB of pack
  data on linux. The newest ADR 0005 amendment is right on cost, for a different
  reason than it gave.
- `git repack -a` without `-l` reads 6.6 GiB on linux. `gc.auto=0` and
  `maintenance.auto=false` are required, not advisory.

Unmeasured and load-bearing: pack retention. A projected `objects/` means the
gateway must not delete a pack a live mount references. Git's own answer to that
race is a grace period rather than locking, and GFS's lease would have to widen
from "the pinned commit" to "the mount's pinned ref view". That is the main new
engineering cost and nothing here tests it.
