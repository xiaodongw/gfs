# Searching a GFS workspace: instructions for agents and tools

Status: current as of M9 (ADR 0009). `gfs find` and `gfs log` are **retired**:
the workspace has a real `.git` over a projected object store, so `git ls-files`
and `git log` answer natively and cost blocks proportional to the question.  
Companion: [DESIGN.md](DESIGN.md) section 7.5, [ADR 0004](adr/0004-search-representation.md),
[ADR 0005](adr/0005-git-command-surface.md)

An GFS workspace looks like an ordinary directory tree, and almost every tool
that reads it works. **Asking a question about the whole repository is the
exception**, and this document is why.

The questions an agent asks constantly cost far more inside a mount than they
look like they should, because each one wants to touch every path — or every
commit:

| Question | Do not run | Run |
| --- | --- | --- |
| what does this content say? | `rg pattern` | `gfs rg pattern` |
| which files are called this? | `find . -name` | `git ls-files '<glob>'` — free: the index is local |
| what changed recently? | — | `git log` — answered through the projection, commit-graph backed |
| what did this commit change? | `git show --stat` on a monorepo | `gfs show <rev>` — a tree diff moves ~90 MiB wherever it runs; the gateway already has the objects |
| what changed between these two? | `git diff a b` on a monorepo | `gfs diff <a> <b>` |
| who last touched this line? | `git blame` on a monorepo | `gfs blame <path>` — measured 196 MiB through the projection |

All of them are answered by the **server**, which has the object database and the
search index; none of them reads the mount. A warm run of all of them against
django leaves hydration at 0 blobs and 0 bytes. See
[`benchmarks/agent-workflow.md`](../benchmarks/agent-workflow.md) for the
measurements, including what the workflow cost before these tools existed.

Each accepts `--workspace <path>` for an orchestrator that is not standing in the
mount, and finds the workspace on its own otherwise.

## `HEAD` is the pin

Every command here takes a revision, and in a workspace `HEAD` means the commit
this view is **pinned to** — not the repository's default branch. After
`gfs switch -c` those are different commits, and the distinction is the
difference between reviewing your own work and reviewing someone else's.

Ancestry expressions work from it: `HEAD~3`, `main^`, `abc1234^2~1`. `~n` walks
n first parents and `^n` picks the n-th parent of one commit; they coincide
until you reach a merge. Anything further into `git rev-parse`'s expression
language — `main^{tree}`, `HEAD@{2}`, `a..b`, `:/text` — is **refused**, because
those carry object-type and reachability semantics GFS does not expose and
`^{tree}` in particular would hand back a tree where a commit is expected.

## Use `gfs rg`, not `rg`

Running `rg` inside the mount walks every directory and reads every file. On the
worst-case repository in the M0.1 corpus that is 94 751 first-time filesystem
lookups and a download of the entire tree — it turns the operation an agent runs
most often into the most expensive thing available, and there is no partial
version of that cost.

`gfs rg` answers the same question from the server's index of the pinned commit
plus the workspace's own edits. It downloads nothing from the base.

```
gfs rg 'fn authorize' src/
gfs rg -F 'TODO(' --json
gfs rg -i needle -g '*.rs'
```

`gfs search --workspace <path> <pattern>` is the same search with an explicit
workspace, for an orchestrator that is not standing inside the mount.

## Use `git ls-files` and `git log` — they are free now

ADR 0009 gave the workspace a real `.git`: a local index and a projected
object database. That retired `gfs find` and `gfs log`, because the questions
they answered are what Git answers natively:

```
git ls-files '*.py'                    # the index is local disk: zero fetches
git ls-files -- 'src/**/*.rs'
git log --oneline -20                  # commit-graph backed, ~0.2 MiB of blocks
git log -10 -- src/flask/cli.py        # changed-path Bloom filters make this cheap
git log -3 -p
```

The costs were measured (`spikes/reports/m05b-git-projection.md`): `ls-files`
touches the projection not at all, and a bounded `log` fetches blocks
proportional to the question. Two forms remain expensive on a monorepo and are
refused or better asked of the server: `git gc`/`repack`/`fsck` are refused by
the shim outright, and `git blame` / `git show --stat` work but move 90–196 MiB
through the projection — `gfs blame` and `gfs show` answer server-side for
free.

One behavioural note that survives from the old tools: `git ls-files` lists the
**index** — a file you created is `??` in `git status` and unlisted until
`git add`, and a file you deleted stays listed until the deletion is staged.
That is stock Git's contract, which is the point.

`-S`, `--follow`, `--graph` and `--topo-order` are **refused**, not
approximated. The first two search *across* commits rather than comparing each
to its parent and are not bounded by the page size; the last two need the
reachable graph buffered before anything can be printed. To search content use
`gfs rg`; for tooling that genuinely needs full history, clone through the
smart-HTTP gateway.

## Use `gfs show` and `gfs diff <a> <b>`, not `git show`

The workspace has no object database, so nothing local can say what a commit
changed. Both of these are rendered by the gateway with libgit2 — Git's own
patch format, byte-exact enough to pipe into `git apply` — and download nothing.

```
gfs show                          # the pinned commit, with its patch
gfs show HEAD~2 --stat            # one commit, by file
gfs show abc1234 -- src/          # limited to a path
gfs diff HEAD~3 HEAD              # a range, as one patch
gfs diff HEAD~3..HEAD --name-status
gfs diff                          # no revision: the workspace's own edits
```

Rendering flags are shared by `show`, `diff` and `log -p`: `-p/--patch`,
`--stat`, `--name-only`, `--name-status`, and `-U<n>`/`--unified=<n>`.

**Merges.** `git show` prints a merge's header and no diff at all. `gfs show`
defaults to the first parent — what the merge brought in — with `--parent 2` for
the side branch and `-m` for every parent in one run. The two are usually very
different in size, and that difference is the thing to look at.

**A root commit** is diffed against the empty tree, so the first commit in a
history is reviewable rather than unreachable.

**`gfs diff` with one revision** compares it against the pin, not against the
working tree the way `git diff <commit>` does — answering that exactly would
mean merging the overlay's edits into a server-rendered patch. When the
workspace is dirty it says so on stderr and names the command that shows the
rest.

A very large diff is **truncated with a note on stderr** rather than failing.
The note is on stderr and not in the patch, so a truncated patch piped into
`git apply` does not fail for a confusing reason — but do not mistake it for the
whole diff.

## Use `gfs blame`, not `git blame`

```
gfs blame src/flask/cli.py
gfs blame src/flask/cli.py -L 40,80
gfs blame README.md --rev HEAD~5
```

The path may be relative to where you are standing. A binary file, a directory
or a submodule is refused rather than attributed, and a file over the
searchable-blob limit (8 MiB) is too large to blame.

## `gfs ls` and `gfs cat`

Inside a workspace neither needs `--repo`, and both default to the pin.

```
gfs ls src/flask
gfs cat --rev HEAD~5 src/flask/cli.py
```

**The path column is root-relative in every position.** Listing the root prints
`src`; listing `src` prints `src/flask`. At the root the two coincide, which
makes the rule easy to misread as "basename at the top, full path below" —
recursing on that reading builds `src/src/…`. Root-relative is what `gfs cat`
takes, so the output of one is the input of the other.

A path that is not a directory in that commit is an **error with a non-zero
exit**, never an empty listing. The one exception is a submodule, which
DESIGN.md section 8.2 presents as an empty read-only directory because its
contents live in another repository.

## The answer has two dimensions, and both matter

An empty result from an ordinary search tool is ambiguous: it could mean the
symbol is absent, or that the query was cut short. An agent that cannot tell
those apart will conclude a symbol does not exist and act on it.

Every GFS search therefore reports two independent facts:

**Execution status** — did the query finish evaluating the searchable corpus?
`COMPLETE` or `TRUNCATED`. A budget, or a pattern the index cannot bound, makes
it `TRUNCATED`.

**Coverage** — what was outside the corpus *within the scope you asked about*?
Reported as counts by reason: `binary`, `oversized`, `invalid_utf8`,
`generated`, `vendored`, `index_gap`.

### Exit codes

| Exit | Meaning | What to do |
| ---: | --- | --- |
| 0 | Complete; matches found | Use the results. |
| 1 | Complete; no matches | The pattern genuinely does not occur in scope. |
| 2 | The search did not complete | **Do not** treat the output as an answer. Retry. |
| 3 | Truncated by a budget | The results are real but incomplete. Narrow the scope or raise the limit. |
| 4 | A coverage gap, under `--require-exhaustive` | Some paths were not searched. Read the coverage report. |

**Exit 1 and exit 3 are the pair that matters.** Both may return nothing. Only
exit 1 means "not there".

By default a coverage gap is a warning on stderr and does not change the exit
code — binaries and oversized files are normal, and ADR 0004 measured them at
0.02 % of unique blobs on the Linux kernel and 1.40 % on vscode. Pass
`--require-exhaustive` when a negative result must be trusted absolutely, and
handle exit 4.

## What is searched

- **Regular and executable files** in the pinned commit, plus every file the
  workspace has created or modified.
- **Not symlinks or submodules.** `rg` does not follow symlinks by default
  either, so this is the same corpus, not a smaller one.
- **Not binary files** (a NUL byte in the first 8 KiB — ripgrep's own rule) or
  files over 8 MiB. Both are reported in coverage.
- **Ignored files are skipped** the way `rg` skips them, but only when the
  pinned commit does not already track them: an edit to a tracked file inside
  `target/` is still searched. `--no-ignore` searches everything.

Results are ordered by path, then line, then column, and two runs of the same
query return the same answer.

## Your edits are included

Search always reflects the **merged** workspace:

- a file you created is searched from local content;
- a file you edited is searched from local content, and the pinned commit's
  version of it is not reported;
- a file you deleted produces no results;
- a file you renamed reports its matches at the **new** path — and costs nothing
  extra, because the bytes did not change;
- a `chmod +x` changes nothing about the results.

## `gfs rg` refuses flags it does not implement

Ripgrep has flags that change what counts as a match. `gfs rg` implements a
subset and **rejects the rest with an error** rather than ignoring them, because
a silently dropped `-w` returns matches you did not ask for and the wrong answer
looks exactly like a right one.

Supported: `-e/--regexp`, `-F/--fixed-strings`, `-i/--ignore-case`, `-g/--glob`,
`--exclude`, `-A`, `-B`, `-C`, `-m/--max-count`, `--json`, `--no-ignore`,
`--require-exhaustive`.

If you genuinely need a flag that is not on the list, `--hydrate` runs real
ripgrep over the mount. That downloads every file it reads. It is available so
that "unsupported" never means "impossible", not because it is a reasonable
default.

## `--json`

One object per invocation:

```jsonc
{
  "base_commit": "sha1:...",
  "ref_name": "refs/heads/main",
  "local_matches": 2,          // how many came from your edits rather than the commit
  "outcome": "completed",
  "matches": [
    {
      "path": "...",           // base64url: a path need not be UTF-8
      "line": 42,
      "column": 9,             // 1-based, in BYTES, into the file as stored
      "matched": "...",        // base64url
      "line_text": "...",      // base64url
      "before": [], "after": [],
      "blob_oid": "sha1:..."   // empty for a match in local content
    }
  ],
  "completion": {
    "execution_status": "COMPLETE",
    "truncation": null,
    "stop_budget": null,
    "coverage": {
      "scope": "...",          // base64url
      "eligible_paths": 812,
      "excluded": { "binary": 3 },
      "declared_exclusions": ["binary", "oversized"]
    },
    "index_generation": 7,
    "commit": "sha1:...",
    "candidates_considered": 14,
    "bytes_read": 91234,
    "elapsed_ms": 38
  }
}
```

Byte fields are base64url. JSON strings must be valid UTF-8 and a repository path
need not be; encoding them keeps the one field you use to open a file byte-exact
instead of putting U+FFFD in it.

`"outcome": "failed_before_completion"` means the search did not finish and
carries a `reason` instead of a `completion`. Treat it as exit 2: the matches, if
any, are not an answer.

## MCP tool schema

```json
{
  "name": "gfs_search",
  "description": "Search a GFS workspace: the pinned commit's index plus local edits, without downloading the repository. Reports execution status and coverage separately, so an empty result is never ambiguous.",
  "inputSchema": {
    "type": "object",
    "required": ["pattern"],
    "properties": {
      "pattern":  { "type": "string", "description": "A regular expression, or a literal string when fixed_strings is true." },
      "path":     { "type": "string", "description": "Limit the search to this path prefix. Empty searches the whole workspace." },
      "fixed_strings": { "type": "boolean", "default": false },
      "ignore_case":   { "type": "boolean", "default": false },
      "glob":     { "type": "array", "items": { "type": "string" }, "description": "Include only paths matching these globs." },
      "exclude":  { "type": "array", "items": { "type": "string" } },
      "before_context": { "type": "integer", "minimum": 0, "maximum": 64, "default": 0 },
      "after_context":  { "type": "integer", "minimum": 0, "maximum": 64, "default": 0 },
      "max_results":    { "type": "integer", "minimum": 1, "default": 1000 },
      "no_ignore":      { "type": "boolean", "default": false, "description": "Search files the workspace's ignore rules would skip." },
      "require_exhaustive": {
        "type": "boolean",
        "default": false,
        "description": "Fail rather than warn when any path in scope was not searched. Use when a negative result must be trusted."
      }
    }
  },
  "outputSchema": {
    "type": "object",
    "required": ["status", "matches"],
    "properties": {
      "status": {
        "type": "string",
        "enum": ["complete", "truncated", "failed"],
        "description": "complete: the corpus in scope was fully evaluated. truncated: a budget stopped the query; the results are real but incomplete. failed: the search did not finish and the results are not an answer."
      },
      "matches": {
        "type": "array",
        "items": {
          "type": "object",
          "properties": {
            "path": { "type": "string" },
            "line": { "type": "integer" },
            "column": { "type": "integer", "description": "1-based, in bytes." },
            "line_text": { "type": "string" },
            "from_local_edit": { "type": "boolean" }
          }
        }
      },
      "coverage": {
        "type": "object",
        "description": "Paths in scope that were not searched, by reason. Non-empty means the answer has holes; check before concluding something is absent.",
        "properties": {
          "eligible_paths": { "type": "integer" },
          "excluded": { "type": "object", "additionalProperties": { "type": "integer" } }
        }
      },
      "commit": { "type": "string", "description": "The pinned commit the base half was searched at." }
    }
  }
}
```

### The one rule a tool implementer must not get wrong

Map `status` from the completion message, not from the match count. A tool that
reports "no results" for a truncated or failed search re-creates exactly the
ambiguity this whole mechanism removes, and the agent on the other side has no
way to notice.
