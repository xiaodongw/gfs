# ADR 0005: The `.git` surface inside the mount

- Status: Accepted
- Date: 2026-07-26
- Milestone: M0.5
- Evidence: `spikes/git-surface`, `spikes/reports/m05-git-surface.md`

## Context

DESIGN.md section 8.6 offers two options for what occupies `.git` in a mounted
workspace: a synthesized read-only directory plus a `git` shim, or a real
shallow blobless partial clone whose promisor remote is the GFS gateway. It
states the choice is "a measurement, not a preference", and PLAN.md warns the
answer changes the milestone graph — if partial clone wins, the minimum M5
upload-pack/promisor scope becomes a predecessor of M2, and the parallel
14–18 week estimate is off the table.

## Decision

**Synthesized read-only `.git` plus the `git` shim.** The M2 → M5 dependency
does **not** invert; M5 may remain parallel to M3/M4 as PLAN.md section 1 hopes.

### The measurement that decides it

Shallow blobless partial clone of the Linux kernel (94 751 tip files):

| Metric | Value |
| --- | ---: |
| `.git/index` | **9.7 MiB, per job** |
| Index entries | 94 850 |
| **`stat`-family syscalls per `git status`** | **101 180** |
| `git status` wall time on local ext4 | 0.12–0.21 s |
| Working tree checked out | 1540 MiB |
| `.git` total | 298.7 MiB |

0.12 s looks harmless. It is not, because it is 0.12 s *on a local ext4
filesystem*. `git status` stats every index entry, so inside a mount those
101 180 stats become 94 850 distinct first-time FUSE lookups.

M0.2 measured what that costs. The kernel absorbs *repeated* stats completely —
1000 `stat(2)` calls on one path produced 0 upcalls at a 60-second TTL — but
that caching does nothing for the first stat of each of 94 850 distinct paths.
Every one is an upcall. **`git status` becomes a full metadata sweep of the
monorepo, which is precisely the cost GFS exists to avoid**, and an agent runs
it out of habit, repeatedly, in a job whose whole premise is not sweeping the
tree.

The 9.7 MiB index is a second, independent objection: it is per job, written at
mount time, and it is a second view of "what changed" that can disagree with the
overlay journal.

Against that, `gfs status` is derived from the overlay journal and touches no
base metadata at all. The asymmetry is not marginal.

### The synthesized surface must include `objects/` and `refs/`

DESIGN.md section 8.6 specifies `HEAD`, a `packed-refs` entry, a minimal
`config`, and `gfs.json`. **That set is not sufficient.** With exactly those
four files, Git does not recognize the directory as a repository at all and
every command fails with `not a git repository`, so the surface satisfies
nothing.

Adding empty `objects/` and `refs/` directories makes repository detection work.
The MVP surface is therefore: `HEAD`, `packed-refs`, `config`, `gfs.json`,
`objects/`, `refs/`.

### The shim is load-bearing for correctness, not only for hydration control

With the corrected surface, measured behaviour splits three ways:

| Command | Result | Assessment |
| --- | --- | --- |
| `rev-parse --show-toplevel` | works | root discovery, as intended |
| `rev-parse --git-dir` | works | |
| `rev-parse HEAD` | works, correct OID | |
| `rev-parse --abbrev-ref HEAD` | works, correct branch | |
| `symbolic-ref --short HEAD` | works | |
| `status --porcelain` | fails, `bad object HEAD` | fails visibly, as designed |
| `log -1` | fails, `bad object HEAD` | fails visibly |
| `show HEAD:<path>` | fails | fails visibly |
| `cat-file -t HEAD` | fails | fails visibly |
| **`ls-files`** | **exit 0, empty output** | **silently wrong** |
| **`diff --stat`** | **exit 0, empty output** | **silently wrong** |

DESIGN.md claims object-requiring commands "fail immediately and visibly rather
than returning a wrong answer". That holds for four of six — and **not** for
`ls-files` and `diff`, which exit 0 with empty output. A tool asking "what files
are tracked?" is told "none"; a tool asking "what changed?" is told "nothing".
Both are wrong answers that look like right ones, which is the exact failure
mode the design set out to avoid.

The reasoning in DESIGN.md anticipated an *empty index* reporting every file as
deleted. The real behaviour is *no index at all*, which reports nothing —
quieter, and worse.

**Consequence: the `git` shim is mandatory for correctness, not merely a
usability and hydration-control measure.** `ls-files` and `diff` must be
intercepted or they lie. DESIGN.md section 8.6's own caveat — that the shim is
not a security boundary and tools bypassing `PATH` see the raw surface — now
carries a sharper cost, and belongs in the compatibility boundary as a stated
limitation rather than a footnote.

### Frozen shim grammar

Backed by the overlay journal and the snapshot API; every other form exits
non-zero with an actionable message. No `--hydrate` escape hatch exists, because
under the synthesized surface there is no object database to delegate to.

| Subcommand | Supported forms |
| --- | --- |
| `status` | `--porcelain`, `--porcelain=v1`, `--short`, `-z`, plain |
| `diff` | plain, `--stat`, `--name-only`, `--name-status`, `--cached` (empty), `-- <pathspec>` |
| `rev-parse` | `HEAD`, `--show-toplevel`, `--git-dir`, `--abbrev-ref HEAD`, `--is-inside-work-tree`, `--verify HEAD` |
| `ls-files` | plain, `-z`, `--cached`, `-- <pathspec>` |
| `show` | `HEAD:<path>` only |
| `log` | `-1` with `--format=`/`--pretty=` for the pinned commit only |
| `symbolic-ref` | `--short HEAD`, `HEAD` |

Pathspecs are literal paths and simple globs; magic pathspecs (`:(glob)`,
`:(exclude)`) are rejected rather than approximated.

### Amendment, 2026-07-27: two of these questions moved to the server

The grammar above stays as the **shim's** contract, and the shim is unchanged.
What changed is the premise behind two of its narrowest entries.

`log` was frozen at `-1` because the workspace has no object database and there
was no way to produce a second commit. `ls-files` was answerable but only by
listing one directory at a time through the snapshot API, which on django's
7 077 files took 28.9–53.7 s. Both are questions the **server** can answer
directly, and it now does, through two new `SnapshotService` RPCs and two tools
that sit beside `gfs rg`:

| Question | Tool | RPC |
| --- | --- | --- |
| recent history | `gfs log` | `Log` — a libgit2 revwalk, paged by `skip` |
| filenames | `gfs find` | `FindPaths` — a server-side tree walk with globs |

This does not reopen the decision. Nothing is hydrated, no object database
appears in the workspace, and the tools refuse what would require one — `-p`,
`-S`, `--follow`, `--stat` and `--graph` each need a tree or blob per commit,
which is exactly the unbounded download this ADR rejected the partial clone for.
The escape hatch for a workload that genuinely needs full Git is still a clone
through the M5 gateway.

The shim itself is not yet rewired to these tools, so `git log` and
`git ls-files` inside a mount still follow the frozen grammar above and still
cost what they cost. See [`docs/agent-search.md`](../agent-search.md) and
[`benchmarks/agent-workflow.md`](../../benchmarks/agent-workflow.md).

Whether those tools should *also* occupy the standard names on `PATH` — and
whether `find` and `grep` should be intercepted the way `git` is — is
[ADR 0007](0007-tool-surface-in-the-agent-image.md), open and owned by M6.1. It
records one correction due to this ADR either way: the shim exits 128 outside a
workspace rather than forwarding to the real `git`, which this ADR chose to avoid
crippling ordinary Git, but which makes a `PATH`-wide install unsafe.

## Alternatives considered

**Shallow blobless partial clone.** Rejected on the measurement above: a 9.7 MiB
per-job index and a 94 850-entry metadata sweep on every `git status`, landing
exactly where GFS is trying to save. It would make substantially more of Git
work, which is a real loss.

**Synthesized by default with partial clone behind a mount option.** Deferred,
not rejected. It is the natural escape hatch for a workload that genuinely needs
full Git, and it becomes cheap once M5 exists. Not MVP scope: it would pull the
promisor path forward into M2 for a need the pilot has not demonstrated.

**No `.git` at all.** Rejected in DESIGN.md and confirmed here — without it,
every root-detecting tool fails and an agent's likely repair is `git init`,
producing a second, wrong source of truth inside the job.

## Consequences

- M2.1 implements the six-entry synthesized surface, including the two empty
  directories that the design's original list omitted.
- M3.3's shim work must cover `ls-files` and `diff` as **correctness** items;
  they are the two commands that currently return confidently wrong answers.
- The M5 gateway remains parallelizable with M3/M4.
- `safe.directory` remains required whenever the mount is owned by the host
  daemon rather than the job UID; the same-UID case was verified clean, and the
  cross-UID case is the one M2.1 must configure.
- A documented limitation for the compatibility boundary: tools that invoke Git
  by absolute path, bypassing the shim in `PATH`, will get empty rather than
  erroring answers from `ls-files` and `diff`.

## Amendment, 2026-07-26: the shim landed in M2, not M3.3

This ADR's Consequences put the six-entry surface in M2.1 and the shim's
`ls-files`/`diff` work in M3.3. The surface landed where predicted. The shim
landed **one milestone earlier**, in M2, and the reason is this ADR's own
argument.

PLAN.md M2.4 requires exercising "the `.git` surface and `git` shim", including
"confirmation that unsupported subcommands fail with an actionable message
instead of a wrong answer". A shim that did not exist could not be exercised, and
the alternative — testing the raw surface alone — would have signed off a
milestone knowing that `ls-files` and `diff` return confidently wrong answers.

`gfs-fuse/src/bin/gfs-git-shim.rs` implements the frozen grammar in full.
`status`, `diff`, and `ls-files` currently answer from the mount and from the
fact that a read-only mount has no local changes, which is correct rather than
approximate: there is no overlay yet to differ from the base. **M3.3's work is
unchanged in substance and reduced in scope**: rewire those three to the overlay
journal, and add the cases that only exist once something can be edited.

Two details settled by building it:

- **The shim needs no credential.** The daemon calls `GetCommit` once at mount
  time and embeds the result in `.git/gfs.json`, so bounded `log -1` reads a
  local file. A shim that called the server would carry the mount capability,
  and a `PATH`-installed wrapper any process can invoke is the wrong place for
  one.
- **It refuses outside a GFS workspace.** Installed early in `PATH` it is
  invoked everywhere; answering for an ordinary Git repository would replace a
  working `git` with a crippled one. It searches upward for `.git/gfs.json`
  specifically.

The measured behaviour this ADR recorded was re-verified against the current
stock Git rather than assumed: `compat.rs::stock_git_ls_files_and_diff_are_silently_empty_which_is_why_the_shim_exists`
asserts that `ls-files` and `diff` still exit 0 with empty output, so the shim's
justification is a live test rather than a historical note.
