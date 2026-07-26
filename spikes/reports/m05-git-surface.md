# M0.5 — Git-command surface inside the mount

Milestone exit gate (PLAN.md M0.5):

> a decision backed by measured `git status` cost on the worst-case repository,
> and a list of tools that the chosen option does not satisfy.

**Met.** Decision: synthesized read-only `.git` plus the `git` shim. The M2 → M5
dependency does not invert.

Decision and frozen shim grammar: [ADR 0005](../../docs/adr/0005-git-command-surface.md).

## How to reproduce

```sh
cd spikes
./git-surface/measure.sh linux     # the decisive run
./git-surface/measure.sh vscode
```

## The decisive measurement

Shallow blobless partial clone of the Linux kernel, the worst case in the
corpus:

| Metric | linux | vscode |
| --- | ---: | ---: |
| Working-tree files | 94 751 | 16 862 |
| `.git/index` | **9.7 MiB** | 2.2 MiB |
| Index entries | 94 850 | 16 863 |
| **`stat`-family syscalls per `git status`** | **101 180** | 21 293 |
| `git status`, first | 0.21 s | 0.40 s |
| `git status`, steady state | 0.12 s | 0.06 s |
| `git diff --stat`, 5 edited files | 0.03 s | 0.01 s |
| Clone wall time (local `file://`) | 19.6 s | 4.7 s |
| Working tree checked out | 1540 MiB | 227 MiB |
| `.git` total | 298.7 MiB | 53.2 MiB |

0.12 s for `git status` looks harmless, and taken alone it would argue *for*
the partial clone. It is misleading, because it is 0.12 s **on local ext4**.

`git status` stats every index entry. Inside a mount those 101 180 stats become
94 850 distinct first-time FUSE lookups. M0.2 measured exactly what the kernel
does and does not absorb: 1000 repeated `stat(2)` calls on one path produced
**0** upcalls at a 60-second attribute TTL, but that caching does nothing for
the *first* stat of each distinct path. Every one is an upcall.

So `git status` under the partial-clone option is a full metadata sweep of the
monorepo — the specific cost XVFS exists to eliminate — run by agents out of
habit, repeatedly, in a job whose premise is not sweeping the tree. Against
that, `xvfs status` is derived from the overlay journal and touches no base
metadata at all.

The 9.7 MiB per-job index is a separate objection: it is written at mount time,
it scales with the repository rather than the job, and it is a second view of
"what changed" that can disagree with the overlay journal.

## Behaviour of the synthesized surface

> **The specified contents are not sufficient.** DESIGN.md section 8.6 lists
> `HEAD`, `packed-refs`, `config`, and `xvfs.json`. With exactly those four
> files Git does not recognize the directory as a repository at all and every
> command below fails with `not a git repository`. Empty `objects/` and `refs/`
> directories are also required. The MVP surface is six entries, not four.

With the corrected surface, on both repositories:

| Command | Result | Assessment |
| --- | --- | --- |
| `rev-parse --show-toplevel` | works | as intended |
| `rev-parse --git-dir` | works | |
| `rev-parse HEAD` | works, correct OID | |
| `rev-parse --abbrev-ref HEAD` | works, correct branch (`master` / `main`) | |
| `symbolic-ref --short HEAD` | works | |
| `status --porcelain` | fails, `bad object HEAD` | visible failure |
| `log -1` | fails, `bad object HEAD` | visible failure |
| `show HEAD:<path>` | fails | visible failure |
| `cat-file -t HEAD` | fails | visible failure |
| **`ls-files`** | **exit 0, empty** | **silently wrong** |
| **`diff --stat`** | **exit 0, empty** | **silently wrong** |

Root discovery works, which is what most non-Git tooling actually needs, and
four of six object-requiring commands fail loudly as the design intends.

But `ls-files` and `diff` **exit 0 with empty output**. A tool asking "what is
tracked?" is told "nothing"; a tool asking "what changed?" is told "nothing
changed". Those are wrong answers that look like right ones — the exact failure
mode DESIGN.md set out to avoid when it argued the surface would "fail
immediately and visibly rather than returning a wrong answer".

The design anticipated an *empty index* reporting every file as deleted. The
real behaviour is *no index at all*, which reports nothing: quieter, and worse,
because a loud wrong answer gets noticed.

**This makes the `git` shim mandatory for correctness, not merely for usability
and hydration control.** It also sharpens DESIGN.md's own caveat that the shim
is not a security boundary: a tool that invokes Git by absolute path, bypassing
`PATH`, gets the silently-empty answers rather than an error.

## What the corpus actually invokes

`git` invocations in build scripts, CI configuration, and tooling at each
repository's tip:

| linux | rust | vscode |
| --- | --- | --- |
| `clone` 10 | `config --global` 25 | `status` 137 |
| `commit` 8 | `add` 11 | `commit` 78 |
| `log` 8 | `fetch` 8 | `diff` 63 |
| `checkout` 4 | `rev-parse` 7 | `branch` 41 |
| `status` 3 | `diff --exit-code` 7 | `remote` 40 |
| `rev-parse` 3 | `checkout` 7 | `push` 39 |
| `diff` 3 | `clone --depth` 5 | `checkout` 30 |

> The vscode counts are inflated: it ships a Git extension, so prose like "git
> repository" and "git state" matches the same pattern. The ordering is still
> informative, and `status`, `diff`, `rev-parse`, and `log` dominate the
> read-only traffic across all three — which is what the frozen shim grammar
> covers.

Commands outside the shim grammar that appear in real config — `checkout`,
`fetch`, `add`, `commit`, `push`, `config --global` — are all write or network
operations. Under the synthesized surface they fail with an actionable message
rather than being approximated, which is the intended boundary.

## Ownership and `safe.directory`

Verified for the same-UID case: a mount owned by the invoking UID is accepted
with no configuration. The cross-UID case is the one that matters for the
host-daemon model chosen in ADR 0003 — a mount owned by the daemon and
bind-mounted into a job running as a different UID triggers Git's
`dubious ownership` check and requires `safe.directory`. That case could not be
exercised here (no root to create a second UID on the host), so it is confirmed
by mechanism rather than end to end, and remains an M2.1 implementation item.

## Limitations

- FUSE metadata operations for `git status` are **derived**, not directly
  measured: the mount was not populated with the Linux tree, so the figure is
  index entries × one first-time lookup each, combined with the M0.2 caching
  result. A direct measurement belongs in M2.4 once the mount can serve a real
  snapshot.
- Clone times are local `file://` and exclude network transfer entirely.
- The shim itself is specified here, not implemented; M3.3 implements it against
  real Git over a materialized checkout.
- `git status` was measured with Git's default settings. `core.untrackedCache`
  and `core.fsmonitor` can reduce the sweep substantially and were not
  evaluated; neither is available through a FUSE mount without further work,
  which is why they do not change the decision.
