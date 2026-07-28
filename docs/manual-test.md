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

## One host, many workspaces

`gfs clone` does not start a daemon per workspace. The first one starts a
`gfs-fuse` **host** on `$XDG_RUNTIME_DIR/gfs/host.sock`, and every later clone
asks that host for another mount:

```sh
gfs clone file:///path/to/one.git
gfs clone file:///path/to/two.git
pgrep -a gfs-fuse        # one process
gfs daemon status        # two mounts
```

```
socket     /run/user/1000/gfs/host.sock
pid        1669729
mounts     2
  /tmp/lab/one  tmp_lab_one  sha1:8b0b4b68…  gen 1  Healthy
  /tmp/lab/two  tmp_lab_two  sha1:5d71e4de…  gen 1  Healthy
```

Each workspace still has its own control socket beside it, so every command
below — `gfs status`, `gfs rg`, the `git` shim — works from inside the tree with
no flags, exactly as it did when each mount had its own process. `gfs unmount`
drops one workspace and leaves the rest of the host running; `gfs daemon stop`
unmounts everything and stops the process. See ADR 0008.

Two mounts of the *same* repository share one blob cache, so the second one pays
nothing for blobs the first already fetched and `--cache-quota` is a per-host
budget rather than a per-mount one.

## Prerequisites

- `cargo build` — the script builds for you if `target/debug/gfs-server` is
  missing, but a release build is faster to poke at:
  `cargo build --release -p gfs-cli -p gfs-fuse -p gfs-server`.
- FUSE: `/dev/fuse` present and `fusermount3` on `PATH` (ADR 0003).
- Network access to whatever you clone. Any URL `git clone` accepts works; a
  local `file:///path/to/bare.git` works too and is the fastest way to try the
  write path without touching a real forge.

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

**Your shell's directory goes stale here.** The workspace is a symlink into
`<workspace>.gfs/generations/<n>`, and committing publishes a new generation and
retires the old one — so a shell standing in the old one gets `ENOENT` on its
next command. `cd $(pwd)` recovers. The same applies to `gfs switch` and
`gfs refresh`. This is inherent to the generation model: PLAN.md M2.1 requires
that a refresh never mutate the pinned base under existing kernel dentries.

### `gfs push`

The gateway pushes outward to the real Git server with **your** credential
(`--credential`, or `GFS_UPSTREAM_CREDENTIAL`), so upstream sees you and not the
service. `refs/gfs/work/<you>/<branch>` is mapped to `refs/heads/<branch>` there.

Pushing *to* the gateway with stock `git push` is still refused, deliberately —
this is the mirror acting as a Git client against the real server, not the
gateway accepting a push.

## The three server-answered tools

An agent asks three questions that want to touch every path. All three are
answered by the gateway; none reads the mount.

```sh
gfs log -5 --oneline
gfs find '*.py'
gfs rg -F 'some-symbol' -m 5
gfs inspect | grep hydration     # still 0 blobs, 0 bytes
```

`gfs log` prints and then, on stderr, `more history follows; continue with
--skip 5`. `gfs rg` prints matches plus a coverage note on stderr about binary
files skipped — that note is deliberate, not a warning of failure.

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

For history and filenames use `gfs log` and `gfs find`; the shim is not wired to
them.

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

Ctrl+C in the terminal running `scripts/dev-server.sh`. That stops the gateway
and unmounts every workspace under the lab directory.

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
- **Your shell's directory goes stale after `switch`, `commit` and `refresh`.**
  `cd $(pwd)`.
- **Do not `pkill -f gfs-fuse`.** The pattern can match the shell running it, and
  `pgrep -c` counts that match too — which reads as a daemon that will not die.
  Kill by PID.
- **`du` on a live mount walks the FUSE tree** and reports ~45 MiB for Django.
  The real client-side state is around 100 KiB, most of it the installed shim
  binary: `du -sb --exclude=generations <workspace>.gfs`.
- **`core.autocrlf=true`** in your global Git config makes anything you clone
  with shell scripts in it arrive with CRLF endings and fail in ways that look
  like the cloned project is broken. It also corrupts `git apply`.
- **A high-ref repository takes tens of seconds to import the first time.**
  Django's 29 298 refs take about 50 seconds; restarting against the same lab
  takes 0.2 s.
