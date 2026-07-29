# Manual testing: driving GFS by hand

One command to start a gateway, then `gfs` works with no configuration. Ctrl+C
when you are done.

```sh
scripts/dev-server.sh
```

In another terminal:

```sh
export PATH="$PWD/target/debug:$PATH"
cd ~/.gfs-lab

gfs clone https://github.com/pallets/flask.git
cd flask
gfs status
gfs log -10 --oneline
gfs switch -c my-change
echo "a change" >> README.md
gfs commit -m "a change"
gfs push
```

That is the whole loop. Everything below is detail about what each step is doing
and what is worth looking at while you do it.

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

**Keep the lab path short.** A workspace's control socket is
`<workspace>.gfs/control.sock` and a Unix socket path cannot exceed 108 bytes.
The default is short; a lab under a deep scratch directory fails to serve, and
the failure reads as something else entirely.

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

### `gfs switch -c <branch>`

Creates the branch **on the gateway** and re-points this view at it. Unpushed
work lands in `refs/gfs/work/<you>/<branch>`, not `refs/heads/`, because the
mirror's fetch runs `--prune` over `refs/heads/*` — a branch there that upstream
does not have is deleted by the next sync, taking every commit on it.

You can see where it went:

```sh
git --git-dir=~/.gfs-lab/repos/*.git for-each-ref refs/gfs/work
```

### `gfs commit -m`

Builds the tree from the overlay journal, makes the commit in the mirror, and
re-pins the view to it. There is no staging area and no local-only commit: the
commit is durable on the gateway the moment it is made, which is what lets
`gfs log` show it.

**Your shell keeps working.** The workspace is the mount point itself and the
re-pin happens in place, so a shell — or an agent CLI, or a build — standing
anywhere inside it is standing in the same place afterwards. The same applies to
`gfs switch` and `gfs refresh`.

Two things behave the way `git switch` behaves, and are worth knowing:

- a working directory that exists on the old commit and not on the new one gives
  `ENOENT` afterwards, exactly as it would after `git switch`;
- a file the new commit *adds* can read as absent for up to a second if something
  looked for it just before the switch. Negative dentries are not enumerable, so
  they expire rather than being invalidated.

Before 2026-07-28 the workspace was a symlink into
`<workspace>.gfs/generations/<n>` and this section said the opposite. See ADR
0003's second amendment.

### `gfs push`

The gateway pushes outward to the real Git server with **your** credential
(`--credential`, or `GFS_UPSTREAM_CREDENTIAL`), so upstream sees you and not the
service. `refs/gfs/work/<you>/<branch>` is mapped to `refs/heads/<branch>` there.

Pushing *to* the gateway with stock `git push` is still refused, deliberately —
this is the mirror acting as a Git client against the real server, not the
gateway accepting a push.

## The server-answered tools

An agent asks a handful of questions that want to touch every path, or every
commit. All of them are answered by the gateway; none reads the mount.

```sh
gfs log -5 --oneline
gfs find '*.py'
gfs rg -F 'some-symbol' -m 5
gfs inspect | grep hydration     # still 0 blobs, 0 bytes
```

`gfs log` prints and then, on stderr, `more history follows; continue with
--skip 5`. `gfs rg` prints matches plus a coverage note on stderr about binary
files skipped — that note is deliberate, not a warning of failure.

### Reviewing history

"What did the last three commits change" is a question the workspace cannot
answer for itself — it has no object database (ADR 0005) — so all of this is
rendered by the gateway and downloads nothing:

```sh
gfs log -3 -p                     # each commit with its diff
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
- **`gfs log -- <path>`** shows only commits that touched a path, and limits any
  patch to it. Rename following (`--follow`) is not implied; that is a
  similarity search per commit rather than a tree comparison.

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

### Glob syntax is gitignore's, and this is the part that surprises people

| Pattern | Matches |
| --- | --- |
| `?` | one byte that is not `/` |
| `*` | zero or more bytes, **none of them `/`** |
| `**` | zero or more path components, `/` included |

**A pattern with no `/` in it matches the file name, not the whole path.** On a
Django checkout, where `admin/options.py` exists:

```sh
gfs find '*options.py'            # 3   -- basename match
gfs find '*/admin/options.py'     # 0   -- `*` cannot cross `/`
gfs find '**/admin/options.py'    # 2   -- `**` can
```

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

## The `git` shim

```sh
export PATH="$(gfs install-shim):$PATH"
git rev-parse HEAD          # works
git ls-files | wc -l        # the tracked set
git log -3 --oneline        # REFUSED, exit 2, prints the supported grammar
git rev-parse --short HEAD  # also refused: the grammar is frozen and narrow
```

The refusals are the designed behaviour. Without the shim, stock `git ls-files`
inside a mount exits **0 with empty output** — it reports that nothing is
tracked — which is why ADR 0005 calls the shim a correctness measure rather than
a convenience:

```sh
/usr/bin/git ls-files | wc -l   # 0 files, exit 0 -- reports nothing is tracked
```

For history, filenames and review use `gfs log`, `gfs find`, `gfs show`,
`gfs diff` and `gfs blame`; the shim is not wired to them.

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

## Two behaviours worth checking by hand

Both were defects found by `spikes/conformance/pjdfstest.sh` and fixed on
2026-07-27; see [`reports/posix-conformance.md`](reports/posix-conformance.md).

**A directory's timestamps advance when its contents change.** Build systems and
watchers rely on this.

```sh
mkdir -p dtest
a=$(stat -c %Y dtest); sleep 1.1; touch dtest/x; b=$(stat -c %Y dtest)
[ "$b" -gt "$a" ] && echo "advanced" || echo "INERT -- regression"
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
place and empty. An empty workspace beside a `.gfs` state directory is what a
released mount looks like; `mount.json` is gone, which is what tells cleanup the
lease was released.

Unmount before deleting anything by hand. `rm -rf` over a live mount is both
wrong and slow: the base is read-only so every removal fails, and ADR 0003
measured that a mount point outlives its daemon and answers `ENOTCONN` until
something unmounts it.

To start from nothing, `rm -rf ~/.gfs-lab` after the server has stopped.

## Traps

Each of these produced a plausible-looking wrong answer during development.

- **Long lab paths fail the control socket.** Under 108 bytes for
  `<workspace>.gfs/control.sock`. The default lab directory is short; a deep
  `GFS_LAB` is not.
- **A workspace must be empty to mount on.** It is the mount point now, so
  `gfs clone` into a non-empty directory is refused rather than hiding what is
  there — the same rule `git clone` has.
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
- **`du` on a live mount walks the FUSE tree** and reports ~45 MiB for Django.
  The real client-side state is around 100 KiB, most of it the installed shim
  binary: `du -sb <workspace>.gfs`. The state directory no longer contains the
  mount, so nothing has to be excluded.
- **`core.autocrlf=true`** in your global Git config makes anything you clone
  with shell scripts in it arrive with CRLF endings and fail in ways that look
  like the cloned project is broken. It also corrupts `git apply`.
- **A high-ref repository takes tens of seconds to import the first time.**
  Django's 29 298 refs take about 50 seconds; restarting against the same lab
  takes 0.2 s.
