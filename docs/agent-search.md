# Searching an XVFS workspace: instructions for agents and tools

Status: current as of M4  
Companion: [DESIGN.md](DESIGN.md) section 7.5, [ADR 0004](adr/0004-search-representation.md)

An XVFS workspace looks like an ordinary directory tree, and almost every tool
that reads it works. Search is the exception, and this document is why.

## Use `xvfs-rg`, not `rg`

Running `rg` inside the mount walks every directory and reads every file. On the
worst-case repository in the M0.1 corpus that is 94 751 first-time filesystem
lookups and a download of the entire tree — it turns the operation an agent runs
most often into the most expensive thing available, and there is no partial
version of that cost.

`xvfs-rg` answers the same question from the server's index of the pinned commit
plus the workspace's own edits. It downloads nothing from the base.

```
xvfs-rg 'fn authorize' src/
xvfs-rg -F 'TODO(' --json
xvfs-rg -i needle -g '*.rs'
```

`xvfs search --workspace <path> <pattern>` is the same search with an explicit
workspace, for an orchestrator that is not standing inside the mount.

## The answer has two dimensions, and both matter

An empty result from an ordinary search tool is ambiguous: it could mean the
symbol is absent, or that the query was cut short. An agent that cannot tell
those apart will conclude a symbol does not exist and act on it.

Every XVFS search therefore reports two independent facts:

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

## `xvfs-rg` refuses flags it does not implement

Ripgrep has flags that change what counts as a match. `xvfs-rg` implements a
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
  "name": "xvfs_search",
  "description": "Search an XVFS workspace: the pinned commit's index plus local edits, without downloading the repository. Reports execution status and coverage separately, so an empty result is never ambiguous.",
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
