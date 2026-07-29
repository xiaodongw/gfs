# History review: rev-aware diff, revision expressions, and the log gaps

## Summary

A "Claude Code CLI on GFS" test run found that GFS can *read* any commit but
cannot say what one **changed**. `gfs log` refuses `-p`/`--stat`, there is no
`gfs show`, and `gfs diff` takes no revisions — so "review the last three
commits" had no first-class answer and the tester hand-rolled a tree-differ in
Python out of `gfs ls --rev` and `gfs cat --rev`.

The report's other findings, each verified by hand against the dev stack on
2026-07-29:

| # | Claim | Verdict |
| --- | --- | --- |
| 1 | No way to see what a commit changed | **Real.** No `show`, no rev-aware `diff`, `log -p/--stat` refused. |
| 2 | `gfs ls` on a bad path is a silent empty success | **Not reproducible.** A missing path is `NOT_FOUND`, exit 1; a file path is `INVALID_ARGUMENT`, exit 1. |
| 2b | `ls` path column is basename at root, root-relative below | **Not a defect.** It is root-relative everywhere; at the root the two coincide. Only the help text was silent about it. |
| 3 | `--repo` required for `ls`/`cat` inside a workspace | **Real.** |
| 4 | No revision expressions (`HEAD~1`, `main^`) | **Real**, and deliberate — but the closed grammar can be extended without reopening `revparse`. |
| 5 | `--format` has no `%b`/`%B` and no date verbs | **Real.** |
| 6 | Merge commits have no first-parent handling | **Real**, and downstream of #1. |
| 7 | No `gfs blame`, no `gfs log -- <path>` | **Real.** |
| 8 | `cd <dir> && python3 x.py` produced no output; cwd was reset | **Not GFS.** The harness restores the shell's cwd after every `cd`; the same message appears here with no mount involved. |

The through-line is that GFS treats "needs a tree or blob per commit" as
forbidden. That rule was written about the *client* — ADR 0005 rejected the
partial clone because the workspace would hydrate itself a piece at a time. The
**server** holds the object database and pays no such cost. Everything below is
answered server-side and downloads nothing to the mount.

## Plan

### Phase 1 — `gfs-types`: revision expressions

`RevisionExpression` = a `RevisionSelector` plus a chain of ancestry steps
(`~n`, `^n`). `RevisionSelector::parse` keeps rejecting `~` and `^`; the new
type strips the operator chain first and delegates the base. `^{`, `@{`, `..`,
`:` and the rest of `revparse` stay rejected, so `main^{tree}` cannot yield a
tree OID where the system expects a commit.

### Phase 2 — `gfs-git`

* `walk_ancestry` — `commit.parent(n)` applied step by step. Not `revparse`.
* `diff` — libgit2 `diff_tree_to_tree` rendered as `patch`, `stat`,
  `name-status` or `name-only`, bounded by a byte cap that reports truncation.
* `log` grows a `LogOptions { skip, limit, first_parent, paths }`.
* `blame` — libgit2 `blame_file`, plus the blamed blob's bytes.

### Phase 3 — proto and service

`DiffCommits` and `Blame` RPCs on `SnapshotService`; `LogRequest` gains
`first_parent` and `paths`; `LogCommit` gains `tree_oid`. `DiffCommits`
authorizes **both** commits.

### Phase 4 — `gfs-mount`: the daemon

Control requests `DiffRevs` and `Blame`, and `Log` gains the new options. The
daemon rewrites a leading `HEAD` to the **pinned commit** before resolving, so
`gfs show HEAD~1` in a workspace on a work branch means what the caller thinks
it means rather than the repository's default branch.

### Phase 5 — the CLI

* `gfs show <rev>` — commit header plus its diff against the first parent, with
  `--parent <n>` and `--all-parents` for merges.
* `gfs diff` — unchanged with no arguments; `gfs diff <a> <b>`, `gfs diff a..b`,
  and `gfs diff <rev>` (against the pin, with a note when the workspace is dirty).
* `gfs blame <path>`.
* `gfs log` — `-p`, `--stat`, `--name-only`, `--name-status`, `--first-parent`,
  `-- <path>...`, and the format verbs `%b %B %T %t %ad %ai %aI %ar %cd %ci %cI %cr`.
* `ls`, `cat`, `resolve` — `--repo` and `--rev` default from the workspace, and
  a leading `HEAD` becomes the pin.
* `gfs-cli/src/gitdate.rs` — Git's date formats, hand-written rather than a
  dependency (see Decisions).

### Phase 6 — docs and tests

`docs/manual-test.md` gets a history-review section; `docs/agent-search.md`
gains the review commands and the `HEAD`-is-the-pin rule; `gfs log`'s "what is
deliberately not here" is rewritten, since most of it is now here. ADR 0005 gets
an amendment, because the refusals removed here were justified by it.

## What changed during implementation

**`gfs ls` and `gfs cat` route through the daemon after all.** The plan had them
staying direct-to-server with only `--repo` defaulted. That broke the moment it
was tested against a real work branch: `gfs commit` puts the commit in
`refs/gfs/work/<you>/<branch>`, which is reachable from no visible ref, and a
direct request for it is refused — correctly, by ADR 0002's object-authorization
rule. Only the mount's own capability opens it, and only the daemon holds one.
So `Ls` and `Cat` joined the control protocol, and the direct path is kept for
an explicit `--repo`, which is what that flag now means: "a repository that is
not necessarily this workspace's".

The failure is worth recording because the half-fix was worse than the original:
before, `gfs ls --repo R --rev HEAD` quietly listed the *default branch* — a
plausible-looking wrong tree. Defaulting `--rev` to the pin turned that into a
visible error, and routing through the daemon turned it into an answer.

**`--flag=value`** is accepted alongside `--flag value` in the argv-parsing
tools. Agents write both, and a refusal by name that rejects a spelling of a
flag it *does* support teaches nothing.

## Decisions

**Rev-aware diff is answered by the server, not by the client.** The report
observed that its Python differ stayed lazy and concluded "the server could just
do it". It can do better than that: libgit2 renders the patch itself, so one
round trip replaces a tree walk plus N blob fetches, and the output is Git's
byte format rather than an approximation.

**`log -p` is N `DiffCommits` calls, not a new field on `LogResponse`.** Putting
patches in the log response would make every log page carry a variable and
potentially enormous body, and the paging contract is already `skip`-based. The
CLI issues one diff per commit it printed — bounded by `-n`, which defaults to
20 — and each is server-side.

**Revision expressions extend the closed grammar rather than opening
`revparse`.** ADR 0006's reason for the closed grammar was never "ancestry is
dangerous", it was that `revparse` accepts an expression language with
reachability and object-type semantics GFS does not intend to expose —
`main^{tree}` being the specific hazard. `~n` and `^n` are parsed here and
applied with explicit parent-index semantics, so nothing new reaches libgit2's
parser. The test that asserted `HEAD~1` is rejected now asserts the dangerous
forms are still rejected.

**`HEAD` in a workspace means the pin.** Sending `HEAD` to the server would
resolve the repository's default branch, which after `gfs switch -c` is not
where the caller is standing. The daemon substitutes its pinned commit.

**`gfs blame` returns the file's bytes with the hunks.** One round trip, and the
alternative — hunks now, blob through the ticketed HTTP path afterwards — buys
nothing for a single bounded file and needs a ticket the control socket has no
reason to mint.

**`gfs ls`'s path column stays root-relative.** It already is, in both positions;
the report's "inconsistent" reading came from the root case, where root-relative
and basename are the same string. Changing it to a basename would break
`gfs cat` on the output. What was missing was the *documentation* — the help
text and `docs/agent-search.md` now state the rule and name the trap.

**Dates are formatted here rather than by a crate.** `%ad`, `%ai` and `%ar` need
one thing: the civil date for a Unix second, which is Hinnant's
`civil_from_days` at about fifteen lines. A date-and-time crate brings a
timezone database, a parser and a serialization format, none of them used, and
every entry in ADR 0001's dependency table has to be audited, licensed and
pinned. Same reasoning as the hand-written Myers diff in `gfs-overlay` and the
hand-written HTTP GET in the CLI.

**Blame returns the file's bytes over gRPC, not through the ticketed HTTP blob
path.** The blob endpoint exists for ranges and revalidation on files the
filesystem reads. A blame is one bounded file read once, and splitting it into
hunks-now plus blob-later costs a round trip and a ticket for nothing.

## Details

Verified against `scripts/dev-server.sh` with `pallets/flask` cloned into
`~/.gfs-lab`, which is the setup `docs/manual-test.md` describes.

The hydration counter is the check that matters for every command added here: it
must stay at `0 blobs, 0 bytes` after a `show`, a `diff`, a `log -p` and a
`blame`, because all four are answered by the gateway.
