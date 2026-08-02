# Manual testing: driving GFS by hand

One command to start a gateway, then `gfs` works with no configuration. Ctrl+C
when you are done.

```sh
scripts/dev-server.sh
```

In another terminal:

```sh
export PATH="$PWD/target/debug:$PATH"   # gfs, plus git/grep/find/rg -- the shims
cd ~/.gfs-lab

git clone https://github.com/pallets/flask.git   # the shim serves this as `gfs clone`
cd flask

git status
git log --oneline -10
find src -name '*.py' | head            # answered from the index, no walk
grep -rn "def route" src | head         # answered by the gateway, no hydration
git switch -c my-change
echo "a change" >> README.md
git commit -am "a change"
git push origin my-change # lands at refs/heads/my-change on the gateway
gfs push my-change        # continues outward to the real Git server
```

That is the whole loop, and apart from the final outward push it *is* the
stock git/grep/find flow — which is the point of ADR 0009: an agent that
knows nothing about GFS works a workspace with the commands it already knows.
`target/debug` builds the shims under the tool names (`git`, `grep`, `find`,
`rg`), so the one `PATH` export is the entire environment setup; `gfs
install-shim` still exists for building the same arrangement into an agent
image. Each shim stands aside whenever it cannot serve you: `git clone` with
flags that do not translate (or no reachable gateway) is a real clone, an
untranslatable `grep`/`find`/`rg` invocation runs the real tool with a stderr
note, and `GFS_SHIM_BYPASS=1` forces the real tool unconditionally.
Everything below is detail about what each step is doing and what is worth
looking at while you do it.

For an automated check instead, `scripts/dev-stack.sh --smoke` brings a stack up
against fixtures and exits non-zero if anything is broken. This document is for
poking at it yourself.

## What the one command does

`scripts/dev-server.sh` starts a gateway on the ports `gfs` already defaults to
(`127.0.0.1:8430` and `:8431`), with a fixed capability key and an **empty** dev
token. `gfs` sends no `Authorization` header when it has no token, so the two
agree and nothing has to be exported — which is the only reason the block above
has no `export GFS_*` lines in it.

That also means **anyone who can reach those ports is `dev`**. It binds to
loopback only and prints the warning on startup. Do not point it at anything you
care about.

Its lab directory is `~/.gfs-lab` (override with `GFS_LAB`). Ctrl+C stops the
gateway *and* unmounts every workspace under it — the `gfs-fuse` host outlives
the gateway by design, and an orphaned mount is what makes the next run fail
confusingly. The host itself is stopped only if the lab's workspaces were the
last ones it was serving, so a mount you made elsewhere survives.

A workspace's control socket lives in the runtime directory
(`$XDG_RUNTIME_DIR/gfs/ws-<hash>.sock`), named by a hash of the workspace path
— it cannot live inside the workspace, because the state sits under the mount
(ADR 0011) and a socket cannot be connected to through a FUSE passthrough. The
authoritative path is recorded in the workspace's own `.git/gfs.json`.

## Prerequisites

- `cargo build` — the script builds for you if `target/debug/gfs-server` is
  missing, but a release build is faster to poke at:
  `cargo build --release -p gfs-cli -p gfs-fuse -p gfs-server`.
- FUSE: `/dev/fuse` present and `fusermount3` on `PATH` (ADR 0003).
- Network access to whatever you clone. Any URL `git clone` accepts works; a
  local `file:///path/to/bare.git` works too and is the fastest way to try the
  write path without touching a real forge.

## One host, many workspaces

`gfs clone` does **not** start a daemon per workspace. The first one starts a
`gfs-fuse` *host* on `$XDG_RUNTIME_DIR/gfs/host.sock`, and every later clone asks
that host for another mount (ADR 0008). The last two lines of a `gfs clone` say
which host answered and where its log is:

```sh
gfs clone file://$HOME/.gfs-lab/alpha.git
gfs clone file://$HOME/.gfs-lab/beta.git
pgrep -c gfs-fuse
gfs daemon status
```

```
1
socket     /run/user/1000/gfs/host.sock
pid        1733998
version    0.1.0
endpoint   http://127.0.0.1:8431
log        /run/user/1000/gfs/host.log
mounts     2
  ~/.gfs-lab/alpha  home_xiaodong_.gfs-lab_alpha  sha1:9768e376…  gen 1  Healthy
  ~/.gfs-lab/beta   home_xiaodong_.gfs-lab_beta   sha1:2e1a575c…  gen 1  Healthy
```

Each workspace still has its own control socket beside it, so everything below —
`gfs status`, `gfs rg`, the `git` shim — works from inside the tree with no
flags, exactly as it did when a mount had a process to itself. `gfs unmount`
drops one workspace and leaves the host serving the rest; `gfs daemon stop`
unmounts everything and stops the process.

**`gfs daemon status` exits non-zero when no host is running**, and does not
start one to answer — otherwise the answer would always be yes. That makes it
usable in a script.

### The shared blob cache is worth seeing

Two views of one repository share a cache, so the second pays nothing for blobs
the first already fetched. `gfs clone` of a URL the gateway holds is a sync, so
this is two workspaces over one mirror rather than two copies — the repository id
in `gfs daemon status` is the same for both:

```sh
gfs clone file://$HOME/.gfs-lab/beta.git beta2
cat ~/.gfs-lab/beta/pkg/options.py  >/dev/null
gfs inspect --workspace ~/.gfs-lab/beta  | grep hydration
cat ~/.gfs-lab/beta2/pkg/options.py >/dev/null
gfs inspect --workspace ~/.gfs-lab/beta2 | grep hydration
```

```
hydration  2 blobs, 54 bytes, 0 cache hits
hydration  2 blobs, 54 bytes, 1 cache hits
```

Same byte count after the second read, and a cache hit instead of a fetch. Under
one daemon per mount each opened its own cache over the same directory, so the
bytes doubled and `--cache-quota` silently meant "per mount" rather than per
host.

## The write path, step by step

### `gfs clone`

Fetches the repository into the **gateway's** mirror and mounts a view of it.
The mirror is the clone; the workspace is a view onto it, the way `git worktree`
is a view onto a repository except that no working files are materialized. That
is why a second `gfs clone` of the same URL is a *sync* rather than a second
copy, and why thousands of views over one mirror is the expected shape.

The mount should appear in well under a second regardless of repository size,
and:

```
hydration  0 blobs, 0 bytes, 0 cache hits
overlay    0 changed paths (0 deleted), 0 of 1073741824 bytes used
```

**Watch the `hydration` line for the rest of this document.** It says whether an
operation actually avoided reading the repository, and it is the one thing a
plausible-looking result cannot fake. Check it any time with `gfs inspect`.

### `git switch -c`, `git commit`

Both are stock Git against the seeded real `.git`: the branch and the commit
are **local** to the workspace's state directory, and the view's pin does not
move. `git status` reads clean after the commit, `git log` shows it, and the
`hydration` line has not moved — committing writes objects, it reads nothing.

Local means local: like any Git checkout, work that has not been pushed exists
in exactly one place. The push below is what makes it durable, and a mount
over leftover local commits is refused rather than re-seeded over them.

### `git push` (to the gateway)

Local commits leave as a pack through the gateway's receive-pack surface, onto
the gateway's **real branches**: the gateway is a fork of its upstream, so
`git push origin my-change` lands at `refs/heads/my-change` exactly as it
would on any other Git host, and the next clone of that branch sees it. The
seeded `.git` carries the `origin` remote and a credential helper that reads
`GFS_TOKEN` from your environment at push time. Against the dev server no
token is needed: the helper presents the empty token, which is the `dev`
subject.

```sh
git push                  # every local branch, through the wildcard refspec
git push origin my-change # just the one
```

Pushed work is safe from syncing because the upstream sync is
**fast-forward-only**: upstream state is fetched into
`refs/remotes/upstream/*`, branches that can follow it do, and a branch that
has diverged — yours ahead, upstream moved, or both — is left where it is and
reported. Resolving the divergence is your job, with the tools Git already
gives you: merge or rebase against `refs/remotes/upstream/<branch>`, or
force-push. Tags and the reserved `refs/gfs/` namespace are not pushable;
everyone sharing the gateway shares its branches, so treat `main` with the
etiquette you would on a shared fork (protected branches are the eventual
refinement).

### `gfs push <branch>`

The gateway pushes the branch outward to the real Git server with
**your** credential (`--credential`, or `GFS_UPSTREAM_CREDENTIAL`), so
upstream sees you and not the service. The branch must be named in the
git-native flow above; after `gfs switch -c` it defaults to the view's own
work branch.

### The server-side alternative: `gfs switch -c`, `gfs commit`

The same write path exists as gateway RPCs, which is what API callers and
`CommitChanges` users drive. `gfs switch -c <branch>` creates the branch **on
the gateway** and re-points the view at it; `gfs commit -m` builds the tree
from the overlay journal, commits in the mirror, and re-pins — no staging
area, durable the moment it is made.

**Your shell keeps working.** The workspace is the mount point itself and the
re-pin happens in place, so a shell — or an agent CLI, or a build — standing
anywhere inside it is standing in the same place afterwards. The same applies
to `gfs refresh`. Two things behave the way `git switch` behaves: a working
directory that exists only on the old commit gives `ENOENT` afterwards, and a
file the new commit *adds* can read as absent for up to a second (negative
dentries expire rather than being invalidated).

Before 2026-07-28 the workspace was a symlink into
`<workspace>.gfs/generations/<n>` and this section said the opposite. See ADR
0003's second amendment.

## Git LFS: the tree is born expanded

The server resolves LFS (ADR 0012), so a workspace arrives in the state
`git lfs pull` would have left behind — and you do not need git-lfs installed
to get it. A small public repository with one tracked file is enough to see
the whole loop:

```sh
cd ~/.gfs-lab
git clone https://github.com/cbeams/lfs-test.git
cd lfs-test

cat .gitattributes         # *.pdf filter=lfs diff=lfs merge=lfs -text
ls -l sample.pdf           # 184292 bytes -- the PDF, not a 131-byte pointer
head -c 8 sample.pdf       # %PDF-1.4
gfs ls                     # where that size comes from
```

```
100644         42 sha1:b634d85f…  .gitattributes
100644       3713 sha1:28ac6392…  README.md
100644     184292 lfs-sha256:b1674191…  sample.pdf
```

The `lfs-sha256:` content key is the whole mechanism visible in one line: entry
metadata reports the expanded size and an LFS key instead of the pointer blob's
git oid, and every consumer downstream of metadata — the mount, `gfs cat`,
WebDAV — expands without knowing it did. The pointer blob is still in the
object store, unchanged, which is what keeps the projection and stock Git
coherent.

**Stock Git stays truthful, and cheap.** The mount seeds a `filter.lfs`
configuration and an index carrying expanded sizes with snapshot-time stat
data, so a clean tree reads clean at zero filter invocations:

```sh
git status                                    # clean
git config --local --get-regexp '^filter\.lfs\.'
```

```
filter.lfs.clean .git/hooks/gfs-lfs-filter clean %f
filter.lfs.smudge .git/hooks/gfs-lfs-filter smudge %f
filter.lfs.process .git/hooks/gfs-lfs-filter process
filter.lfs.required true
```

That configuration is a correctness requirement, not a speed-up: without a
clean filter, `git status` misreports every LFS file and `git add` writes the
expanded content into the object database — the branch-corrupting move.

**Editing re-cleans to a fresh pointer**, which is exactly what git-lfs would
have done:

```sh
printf '%% edit\n' >> sample.pdf
git status --short                  # M sample.pdf
git add sample.pdf
git cat-file -p :sample.pdf         # a fresh pointer: new oid, size 184299
git cat-file -s :sample.pdf         # 131 bytes staged, not 184299
git restore --staged --worktree sample.pdf
git status --short                  # clean; the smudge hydrated the original back
```

The same holds through a commit and a branch switch — `git switch` across
revisions hydrates through the smudge, which is the case that has to work if
"real `.git`" is to mean anything:

```sh
git switch -c lfs-edit
printf '%% edited by hand\n' >> sample.pdf
git commit -am "edit the pdf"
git cat-file -p HEAD:sample.pdf     # the commit holds a pointer, not 184 KB
git switch master
ls -l sample.pdf                    # 184292 again, and `git status` is clean
```

`git push origin lfs-edit` behaves like the section above, and `gfs push`
uploads the branch's new LFS objects through the batch API with your
credential before pushing the ref.

**Search skips LFS entries with their own coverage reason**, beside `binary`
and `oversized` — expanded content is unsearchable by nature and pointer text
would only match on `oid sha256:`:

```sh
gfs rg -F 'PDF'
```

```
gfs search: 2 of 4 paths in scope were not searched: 1 binary, 1 lfs
```

Four things are worth knowing while you poke at this:

- **The first `open()` of a large LFS file blocks for the whole object** —
  fetch, verify, cache write — and the `hydration` line counts it. On a host
  where another workspace already read that object it is a cache hit costing
  nothing (ADR 0008's per-host cache is shared across workspaces and across
  labs, so a second run of this walkthrough shows `0 bytes, N cache hits`
  rather than 184 KB).
- **A smudge writes into the overlay**, so `git switch` and `git restore` on
  an LFS path spend overlay quota at expanded size: `gfs inspect` reports
  `overlay 1 changed paths … 184292 … bytes used` after the steps above even
  though `git status` is clean.
- **"Expanded" is per-entry state.** An object the gateway's store could not
  fetch degrades that one entry to its pointer file, which is the pre-ADR-0012
  behavior: `gfs ls` shows a `sha1:` key and a ~130-byte size for it, and
  reading it gives you the pointer text. Ingest prefetches the default
  branch's tip, so LFS objects that live only on other branches degrade until
  a sync with a credential brings them in. `gfs status` does not yet name the
  degraded entries.
- **git-lfs installed on the host does not collide.** Its global
  `filter.lfs.process = git-lfs filter-process` would otherwise hijack the
  driver and derive a batch endpoint the gateway does not serve, so the
  workspace seeds a *local* `process` entry pointing at the GFS shim, which
  wins Git's precedence. If the mount could not find `gfs-lfs-filter` it warns
  at mount time rather than failing; `filter.lfs.required = true` then makes
  the first use fail loudly instead of silently writing garbage.

## Cheap questions: stock Git for names and history, the gateway for content

The workspace has a real `.git` over the projected object store, so stock Git
answers name and history questions itself; what stays server-side is the
question that would otherwise read file *content* wholesale, or sweep the
tree:

```sh
git log --oneline -5             # commits come through the projection -- cheap
git ls-files | wc -l             # the index is local -- free
gfs find . -name '*.py'          # find's grammar, from the index + overlay journal
gfs rg -F 'some-symbol' -m 5     # content search, answered by the gateway
gfs inspect | grep hydration     # still 0 blobs, 0 bytes
```

Names include refs. The mount writes the gateway's whole filtered ref set as
`packed-refs` — tags with their peel lines, branches as
`refs/remotes/origin/*` — so the questions a version-derived build asks are
answerable locally:

```sh
git tag --list | tail -3         # tags exist; `git describe` works, so hatch-vcs does
git describe --tags
git branch -r                    # refs/remotes/origin/*, as a clone would have
git rev-parse origin/main        # resolves; `git log origin/main` walks
git status -sb                   # `## main...origin/main`, with ahead/behind
git show-ref | grep refs/gfs/    # empty: the reserved namespace is never shown
```

Local branches stay yours: nothing upstream is ever packed into `refs/heads/`.
The view is pinned like the index — `gfs refresh` or a `switch` re-seeds it,
and a ref that moved upstream in between does not move under you.

With the shims installed you rarely type the `gfs` spellings: `rg`, `find`,
and recursive `grep` inside a workspace delegate to `gfs rg`/`gfs find`
automatically, and fall back to the real tool (with a stderr note) when an
invocation uses flags the subset does not honour. `--hydrate`, or
`GFS_SHIM_BYPASS=1`, runs the real tool deliberately.

`gfs rg` prints matches plus a coverage note on stderr about binary files
skipped — that note is deliberate, not a warning of failure. The `git`
invocations move the `odb` counters in `gfs status` rather than the
`hydration` line: commits and trees are small, and that traffic is the
projection working as designed.

### Reviewing history

"What did the last three commits change" is a question stock `git log -p` can
now answer — through the projection, at the price of every blob the diffs
touch. The server-side renderers answer it without downloading anything:

```sh
gfs show HEAD~2 --stat            # one commit, by file
gfs diff HEAD~3..HEAD             # the range as one patch
gfs blame src/flask/cli.py -L 40,80
gfs inspect | grep hydration      # still 0 blobs, 0 bytes
```

Three things about this are worth knowing:

- **`HEAD` is this workspace's pin**, not the repository's default branch. After
  `gfs switch -c` those are different commits, and every command above follows
  the view. Ancestry expressions work from it: `HEAD~3`, `main^`, `abc1234^2`.
- **A merge gets a first-parent diff by default**, which is what `git show`
  refuses to print at all. `--parent 2` gives the side branch and `-m` shows
  every parent in one run — the two are usually wildly different sizes, and that
  difference is the thing worth looking at.
- **Path-limited history is stock Git's now**: `git log --oneline -- <path>`
  walks commits and trees through the projection, which is cheap; adding `-p`
  prices in the blobs of every diff it prints.

`gfs ls` and `gfs cat` need no `--repo` inside a workspace, and default to its
pin:

```sh
gfs ls src/flask                  # root-relative paths, always
gfs cat --rev HEAD~5 src/flask/cli.py
```

The path column is **root-relative in every position**: listing the root prints
`src`, listing `src` prints `src/flask`. At the root the two coincide, which
makes it easy to misread as "basename at the top, full path below" — recursing
on that reading builds `src/src/…`. Root-relative is what `gfs cat` takes, so
the output of one is the input of the other. A path that is not a directory in
that commit is an error with a non-zero exit, never an empty listing.

### Two glob dialects, and this is the part that surprises people

`gfs rg -g` uses ripgrep's globs, which are gitignore's:

| Pattern | Matches |
| --- | --- |
| `?` | one byte that is not `/` |
| `*` | zero or more bytes, **none of them `/`** |
| `**` | zero or more path components, `/` included |

`git ls-files` defaults to the *opposite*: its pathspec `*` crosses `/`, and
`:(glob)` is what restores gitignore semantics. (`gfs find` takes find's own
grammar — `-name` matches a basename, `-path` the whole path — so it is a
third dialect, and the one that matches what you would have typed for `find`.)

```sh
git ls-files '*options.py'           # deep matches -- `*` crosses `/` here
git ls-files ':(glob)*options.py'    # top level only
git ls-files ':(glob)**/options.py'  # gitignore's `**`
```

The same pattern silently over- or under-matches depending on which tool reads
it, which is exactly the shape of wrong answer this document exists to flag.

### The comparison worth making yourself

Needs a repository big enough for the difference to show; Django is a good size.

```sh
rg -F 'ImproperlyConfigured' .    # the wrong way
gfs inspect | grep hydration      # look at what it cost
```

Recorded on Django, 2026-07-27: real ripgrep gives the same answer in about 24
seconds and downloads the entire tree — roughly 6 200 blobs and 47 MB. `gfs rg`
answers in tens of milliseconds and downloads nothing. That difference is the
whole product.

With the shims installed (next section), the first command also prints a
stderr note naming `gfs rg` before running — that note is the degrade rule
working, not a failure.

## The shims: a hint layer, not a grammar

ADR 0005's shim was a frozen grammar that refused most of Git; ADR 0009
retired it. With a real `.git` over the projection, stock Git answers
truthfully, so the shim now routes *cost*, not correctness — the default is
pass-through, and the refused list is five commands that each walk or rewrite
the entire object database:

```sh
export PATH="$(gfs install-shim):$PATH"
git rev-parse --short HEAD  # works -- there is no grammar to fall outside of
git log -3 --oneline        # works
git gc                      # REFUSED, exit 2 (also: repack, prune, fsck, maintenance)
git blame src/flask/cli.py  # runs -- after a stderr note naming `gfs blame`
```

The refusals exist because each of those five, through a projection, is a
wholesale download (`repack -a` measured 6.6 GiB on a kernel-sized
repository). They exit 2, not 1: several Git subcommands use exit 1 as a
data-bearing answer, and a refusal must not be readable as one.

`gfs install-shim` also links `grep`, `find`, and `rg` to the scan shim, which
takes the cheap route rather than advising it: inside a workspace `rg` and
`find` become `gfs rg` and `gfs find` with the same argv, and a recursive
`grep` is translated where the translation is faithful. An earlier version only
printed the cheap route on stderr, which nobody mid-sweep reads. It never
refuses: `gfs rg`/`gfs find` reject an unimplemented flag by name at parse
time, before any output exists, and the shim then execs the real tool with a
note — so an untranslatable invocation works slowly, and the hydration budget
prices whatever it hydrates (`EDQUOT` at open when it runs out). Non-recursive
`grep` runs the real tool untouched, since it reads only the files it was
given. Outside a GFS workspace all four names are fully transparent, which is
what makes a `PATH`-wide install safe.

## The smart-HTTP gateway

Stock Git, with no GFS on the client at all. `<repo-id>` is what `gfs clone`
printed, and `gfs inspect` repeats it:

```sh
git -c protocol.version=2 clone --depth 1 \
  http://127.0.0.1:8430/v1/repos/<repo-id> /tmp/clone
git -C /tmp/clone fsck --no-progress && echo "fsck clean"
```

The internal namespace must not be visible, including the work branch you just
made:

```sh
git ls-remote http://127.0.0.1:8430/v1/repos/<repo-id> | grep -c 'refs/gfs/'   # 0
```

On a server with a real token, an unauthenticated clone gets `401`, and a clone
of a repository you cannot see gets `404` rather than `403` — a distinct status
would answer the existence question.

## The WebDAV surface

Read-only browsing of a branch tip over plain HTTP (ADR 0010). No GFS client,
no `git` — any WebDAV client works, and so does `curl`. The token rides as a
Basic password, the same `x-access-token` convention the gateway takes:

```sh
curl -s -X PROPFIND -H 'Depth: 1' -u "x-access-token:$TOKEN" \
  http://127.0.0.1:8430/dav/ | grep -c '<D:response>'          # 1 + number of repos you may see
curl -s -X PROPFIND -H 'Depth: 1' -u "x-access-token:$TOKEN" \
  http://127.0.0.1:8430/dav/<repo-id>/main/ | grep -o '<D:href>[^<]*'
curl -s -u "x-access-token:$TOKEN" \
  http://127.0.0.1:8430/dav/<repo-id>/main/README.md            # the file bytes
```

A branch with a slash in its name browses as nested folders: `topic/deep` is
`/dav/<repo-id>/topic/` containing `deep/`. Worth checking by hand:

```sh
curl -s -o /dev/null -w '%{http_code}\n' -X PROPFIND -H 'Depth: 1' \
  http://127.0.0.1:8430/dav/                                    # 401 (Basic challenge)
curl -s -o /dev/null -w '%{http_code}\n' -X PUT -u "x-access-token:$TOKEN" \
  http://127.0.0.1:8430/dav/<repo-id>/main/README.md            # 405 (read-only)
curl -s -o /dev/null -w '%{http_code}\n' -X PROPFIND -u "x-access-token:$TOKEN" \
  http://127.0.0.1:8430/dav/<repo-id>/main/                     # 403 (no Depth header = infinity)
```

On a Mac, this surface is the whole point: Finder → Go → Connect to Server →
`http://<host>:8430/dav`, user anything, password the token. The volume mounts
read-only because the server advertises DAV class 1 (no LOCK). Repositories you
cannot read are absent from the listing, and named directly they answer `404`,
exactly like the gateway.

On Windows (validated 2026-07-30 against a WSL2-hosted server, which Windows
reaches at `localhost`): start the redirector once from an admin prompt with
`net start webclient` — without it `net use` fails with system error 67 — then

```bat
net use Z: http://localhost:8430/dav
rem or the redirector's native form; @8430 is how it spells a non-80 port:
net use Z: \\localhost@8430\dav
```

Two traps: `cmd.exe` treats single quotes as literal characters, so the curl
examples above need `-H "Depth: 1"` with double quotes — the mangled header
otherwise reads as missing and earns the `403`; and against a server with a
real token, Windows refuses Basic over plain HTTP by default
(`HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters\BasicAuthLevel`
= 2 allows it, or front the server with TLS). The no-token dev posture avoids
the prompt entirely.

## Two behaviours worth checking by hand

Both were defects found by `spikes/conformance/pjdfstest.sh` and fixed on
2026-07-27; see [`reports/posix-conformance.md`](reports/posix-conformance.md).

**A directory's timestamps advance when its contents change.** Build systems and
watchers rely on this. The workspace root counts as a directory — it was the
exception until 2026-08-02, and the exception is what let a deleted file keep
showing as untracked.

```sh
mkdir -p dtest
a=$(stat -c %Y dtest); sleep 1.1; touch dtest/x; b=$(stat -c %Y dtest)
[ "$b" -gt "$a" ] && echo "advanced" || echo "INERT -- regression"

a=$(stat -c %Y .); sleep 1.1; touch rootprobe; b=$(stat -c %Y .); rm rootprobe
[ "$b" -gt "$a" ] && echo "advanced" || echo "INERT -- regression"
```

**A file that is created and then deleted leaves `git status` clean.** The
intervening `status` is the point: it is what makes Git cache the directory's
untracked extent, and with `core.fsmonitor` configured Git will not re-`lstat`
the directory — it only invalidates the extent when the hook names a path
inside it.

```sh
git status --porcelain            # clean
echo x > probe.txt
git status --porcelain            # ?? probe.txt
rm probe.txt
git status --porcelain            # clean again -- a phantom here is a regression

echo y > staged.txt && git add staged.txt && rm staged.txt
git status --porcelain            # AD staged.txt -- never a bare "A "
git reset -q staged.txt
```

**A too-long path component fails with `ENAMETOOLONG`, not `EIO`.** The limit is
per component (255 bytes), not on the whole path:

```sh
c=$(printf 'a%.0s' $(seq 1 250)); p=""
for i in $(seq 1 16); do p="$p$c/"; done
mkdir -p "${p%/}" && cd "${p%/}"          # now 4022 bytes from the workspace root
touch "$(printf 'b%.0s' $(seq 1 250))"    # File name too long
touch short                               # still works at the same depth
```

## Teardown

Ctrl+C in the terminal running `scripts/dev-server.sh`. That unmounts every
workspace under the lab directory, then stops the host **if the lab's workspaces
were the last ones it had**, then stops the gateway. A workspace you mounted
outside the lab keeps both itself and the host.

The order matters and the script relies on it: the unmount happens while the
gateway is still up, so each mount releases its lease rather than leaving the
gateway holding one until expiry. You can see it in the server log:

```
audit action="release_mount" outcome="ok" mount_id="m-2094a81e77146aae"
```

By hand, from anywhere:

```sh
gfs unmount --workspace ~/.gfs-lab/alpha   # one workspace, host keeps running
gfs daemon stop                            # every workspace, then the host
```

`gfs daemon stop` unmounts each workspace as it goes, leaving the directory in
place holding exactly its own `.git` — a plain working tree with a fat `.git`
is what a released mount looks like (ADR 0011). `.git/gfs/mount.json` is gone,
which is what tells cleanup the lease was released.

Unmount before deleting anything by hand. `rm -rf` over a live mount is both
wrong and slow: the base is read-only so every removal fails, and ADR 0003
measured that a mount point outlives its daemon and answers `ENOTCONN` until
something unmounts it.

To start from nothing, `rm -rf ~/.gfs-lab` after the server has stopped.

## Traps

Each of these produced a plausible-looking wrong answer during development.

- **A workspace must hold nothing besides its own `.git` to mount on.** It is
  the mount point, so `gfs clone` into a populated directory is refused rather
  than hiding what is there — the same rule `git clone` has. A directory
  holding only `.git` is the layout's own shape and is adopted (ADR 0011).
- **Copying a live workspace hydrates it.** `cp -r` on a mounted workspace
  walks the projected tree and downloads what it touches. Unmount first: an
  unmounted folder is self-contained — copy it anywhere and `gfs clone` (or
  `gfs mount`) over it adopts it, relative alternates and all.
- **Killing `gfs-fuse` now kills every workspace, not one.** There is one host for
  the machine, so what used to end a single job now ends all of them. Use
  `gfs unmount --workspace <path>` for one and `gfs daemon stop` for all. (The old
  advice still holds too: `pkill -f gfs-fuse` can match the shell running it, and
  `pgrep -c` counts that match — which reads as a daemon that will not die.)
- **`cargo build` does not restart a running host.** The host is long-lived, so
  after a rebuild it is still the old binary and your change appears to have done
  nothing. `gfs daemon stop`, then any `gfs clone` starts the new one. A build
  that changes the *state directory format* is caught and refused by name; every
  other change is not, and this is the trap.
- **The host outlives the gateway on purpose.** A workspace should survive a
  gateway restart. `gfs daemon status` is how you find one left over from an
  earlier session; it exits non-zero when there is none.
- **`du` on a live mount walks the FUSE tree** and reports the projected
  sizes. There is exactly one place to measure now (ADR 0011): the workspace
  itself. The projection at `.git/gfs/objects` still advertises pack sizes;
  the real local state is what an *unmounted* folder measures.
- **`core.autocrlf=true`** in your global Git config makes anything you clone
  with shell scripts in it arrive with CRLF endings and fail in ways that look
  like the cloned project is broken. It also corrupts `git apply`.
- **A high-ref repository takes tens of seconds to import the first time.**
  Django's 29 298 refs take about 50 seconds; restarting against the same lab
  takes 0.2 s.
