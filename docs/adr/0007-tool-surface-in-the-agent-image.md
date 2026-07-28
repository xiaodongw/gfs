# ADR 0007: The tool surface inside a hosted workspace — same-name shims or distinct names

- Status: **Proposed. Not decided; the decision belongs to M6.1**, which owns the
  agent image, `PATH` precedence, and the agent instructions.
- Date: 2026-07-27
- Milestone: M6.1 (raised while building `gfs log` and `gfs find`)
- Evidence: [ADR 0005](0005-git-command-surface.md),
  [`benchmarks/agent-workflow.md`](../../benchmarks/agent-workflow.md),
  [`docs/agent-search.md`](../agent-search.md),
  `gfs-fuse/src/bin/gfs-git-shim.rs`

## Context

Three questions an agent asks constantly want to touch every path in the
repository: what does this content say, which files are called this, and what
changed recently. GFS answers all three from the server —
[`gfs rg`](../agent-search.md), `gfs find`, `gfs log` — and none of them reads
the mount.

The tools existing is not the same as the tools being *used*. An agent that runs
`rg` out of habit gets the cost GFS exists to remove, and a build script that
runs `git ls-files` gets a wrong answer. So the open question is what occupies
`PATH` inside the hosted image:

- **Same-name shims** — `git`, `rg`, `find`, `grep` on `PATH` ahead of the real
  binaries, each detecting whether it is running inside a GFS mount and either
  answering from the server or forwarding to the real command.
- **Distinct names** — `xgit`, `xrg`, `xfind`, `xgrep` (or the current
  `gfs-*`), with the agent instructed to use them and the standard commands left
  entirely alone.

This ADR records the trade-off. It does not decide it.

## The asymmetry that shapes the answer

The two options are not competing across the board. They are competing for three
of the four commands, because **`git` fails differently from the rest**.

| Inside a mount, running the *stock* command | Result |
| --- | --- |
| `git ls-files`, `git diff` | **Silently wrong** — exit 0, empty output |
| `git status`, `git log`, `git show` | Fails visibly (`bad object HEAD`) |
| `rg`, `grep`, `find` | **Correct, but expensive** — walks and hydrates the tree |

ADR 0005 measured the first row and concluded the `git` shim is "mandatory for
correctness, not merely a usability and hydration-control measure". A tool asking
"what files are tracked?" is told "none"; a tool asking "what changed?" is told
"nothing". `compat.rs::stock_git_ls_files_and_diff_are_silently_empty_which_is_why_the_shim_exists`
keeps that a live test rather than a historical note.

The third row is different in kind. Nothing lies. The cost is real — `rg` inside
the django mount took 23.9 s and hydrated 6 251 blobs / 46.6 MB, against 0.043 s
and zero bytes for `gfs rg` — but a job that pays it still finishes with the
right answer.

**Consequence: `git` is not a candidate for the distinct-name option.** The
processes that run plain `git` are not only the agent. `cargo`, `npm`, `go`,
pre-commit hooks, linters, and language servers all shell out to it, and none of
them will read an instruction telling them to use `xgit`. Whatever is decided for
the other three, `git` needs a same-name shim.

The rest of this ADR is therefore about `rg`, `find`, and `grep`.

## Option A — same-name shims

**For.**

- **Catches habit.** An agent under pressure reaches for the tool it knows. An
  instruction read early in a long session loses to a reflex, and there is no
  second chance to correct it — the cost is paid before anyone notices.
- **Catches everything that is not the agent.** Linters, build steps, language
  servers, and any MCP or subagent tool that shells out. None of these read the
  agent instructions.
- **A refusal arrives at the moment of the mistake**, which teaches better than a
  document read earlier, and is the pattern GFS already uses everywhere: the
  `git` shim prints its whole supported grammar when it refuses; `gfs rg`
  rejects an unknown flag by naming what to do instead.
- **Already half-built and proven** for `git`.

**Against.**

- **Emulation debt, which is where wrong answers come from.** A same-name wrapper
  inherits an interface it must either match or refuse. Two concrete cases:
  - `find` has `-mtime`, `-newer`, `-size`, `-perm`, `-exec`, `-type d`,
    `-maxdepth`, and boolean expressions. `-mtime` and `-newer` are not
    answerable *in principle*, not merely unimplemented: DESIGN.md section 12
    gives every base file the server's sanitized snapshot time, so every file in
    the pinned commit has the same mtime. A wrapper that quietly dropped the
    predicate would answer a different question.
  - `grep`'s POSIX BRE/ERE is not Rust's regex. `grep -r '\(foo\)'` is a group in
    BRE and a literal parenthesis in ERE. Handing either to the trigram engine
    unexamined answers a different question.
- **Interception is invisible when it misbehaves.** A user debugging odd `rg`
  output has to first discover that `rg` is not `rg`.
- **Build tooling is a live hazard.** Agents run builds, and build systems invoke
  `find` and `grep` constantly with flags no wrapper will support. A wrapper that
  refuses breaks the build; a wrapper that forwards silently is slow. Neither is
  obviously right, and the choice cannot be made without knowing what real builds
  actually invoke.
- **It is still not a boundary.** Absolute paths, `execve` from a compiled
  program, shell command hashing, and `$(which grep)` all bypass `PATH`.
  DESIGN.md section 8.6 already states this for `git`; under Option A it applies
  to four commands instead of one, and the gap between "it is handled" and "it is
  usually handled" widens.

## Option B — distinct names

**For.**

- **No emulation debt, therefore no wrong-answer surface.** This is the strongest
  argument and it is not about ergonomics. `gfs find` does not have to decide
  what `-mtime -7` means, because nobody can type it. The tool gets the interface
  the data actually supports rather than one it must partially fake — which is
  the naming-level form of the rule this codebase already applies everywhere:
  refuse rather than approximate.
- **Nothing standard changes meaning.** Builds, linters, and language servers
  keep working exactly as they do outside a mount. Slower, when they sweep the
  tree, but never differently.
- **Debuggable.** A failure in `xrg` is unambiguous about which program failed.
- **Reversible.** Same-name shims can be layered on later; unpicking a same-name
  shim that tooling has come to depend on is harder.

**Against.**

- **Depends on the agent remembering.** This is the whole risk, and it is not
  hypothetical: the failure mode is silent, because falling back to `rg` produces
  a correct answer. Nothing in the output says "you just paid 46 MB for this".
- **Does not cover non-agent processes at all.** Everything in the container that
  is not the agent keeps sweeping the mount.
- **The benefit is invisible when it works and invisible when it does not**,
  which makes the failure hard to notice in a pilot: jobs still succeed, they
  just cost what a clone would have cost, and GFS's value quietly evaporates.

## A third shape, which may be the real answer

The options are not exclusive, and the useful decomposition is **name the tools
by what they are, shim by what the failure is**:

1. Distinct names are the **primary interface** — documented, named in the agent
   instructions, and free of emulation debt.
2. A same-name shim exists **only where stock is wrong rather than slow**:
   `git`. Non-negotiable, per ADR 0005.
3. For `rg`, `find`, and `grep`, same-name wrappers are **guards, not
   replacements**. They do not translate flags. They detect the expensive
   whole-repository shape inside a mount, refuse, and name the tool that answers
   it — plus the explicit opt-in for paying the cost deliberately, which
   `gfs rg --hydrate` already is.

Point 3 keeps Option A's behavioural correction while giving up none of Option
B's safety, because a guard that never translates can never answer a different
question. It is also much less code than partial emulation.

`rg` is the one case where full same-name delegation is low-risk, because
`gfs rg` already implements ripgrep's flag names and exit codes and fails closed
on anything it does not implement. `find` and `grep` are where the emulation risk
lives.

## What would settle it

**Measure what real builds invoke.** The build-tooling hazard is the only
argument here that is currently a guess. Running two or three representative
project builds under an `strace`-style or wrapper-based counter, recording every
`find`/`grep`/`git` invocation and its flags, would replace the guess with a
distribution — and would say whether a scope-aware policy (intercept only
whole-repository shapes; forward anything with explicit file arguments or a
narrow subtree) is workable or fragile.

## Consequences, whichever is chosen

- **The `git` shim must gain a fall-through.** It currently exits 128 with "not
  a GFS workspace" outside a mount (`gfs-git-shim.rs:132`). ADR 0005 chose
  that to avoid "replacing a working `git` with a crippled one", but the effect
  is that installing it on `PATH` breaks ordinary `git` everywhere else in the
  image. Forwarding to the real binary when the workspace is absent is strictly
  better and is required before any `PATH`-wide install.
- **Naming is about to be pinned.** `gfs rg` already appears in
  `docs/agent-search.md`, ADR 0005's amendment, and PLAN.md M6.1's deliverable
  list; `gfs find` and `gfs log` now join it. M6.1 bakes these names into the
  agent image and the agent instructions, after which changing them is a
  compatibility problem rather than an edit. If short names (`xrg`, `xfind`,
  `xgit`) are wanted, decide before M6.1 ships. A cheap middle: keep `gfs-*` as
  the canonical, self-describing namespace and ship short names as symlinks.
- **The MCP tool changes what the CLI is for.** M6.1 already lists an optional
  MCP tool. A tool definition sits in the agent's tool list with a description
  attached and cannot be forgotten the way a CLI convention can, and it can
  return ADR 0004's coverage and truncation signals as structured data rather
  than as an exit code the agent must remember to check. If that lands, the CLI
  wrappers stop being the agent's primary interface and become the safety net for
  everything else in the container — which argues for spending the effort on
  `git` and on light guards, not on polished same-name emulation.

## Alternatives considered

**Instructions only, no wrappers at all.** Rejected as insufficient on its own:
it covers neither non-agent processes nor the `git` correctness problem, which
ADR 0005 already settled.

**Full same-name emulation of `find` and `grep`.** Rejected as the default. The
two cases above — mtime against a sanitized snapshot time, BRE against Rust regex
— are not edge cases but the ordinary use of those tools, and every silent
approximation is the wrong answer that looks like a right one, which is the
failure mode this project has spent four ADRs avoiding.

**Making the mount refuse expensive access at the filesystem layer** (for
example, failing a full-tree `readdir` sweep). Not seriously considered: it would
break correct programs for doing legitimate things, and the FUSE layer cannot
distinguish an agent's habit from a build step's necessity.
