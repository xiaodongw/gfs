# Testing GFS from this bundle

Prebuilt binaries — nothing to build. Two terminals: one runs the server,
one runs the workflow.

## What the target machine needs

- x86_64 Linux, Ubuntu 20.04 or anything newer (the binaries are built in an
  Ubuntu 20.04 container; glibc 2.30+ is the actual floor).
- FUSE: `/dev/fuse` present and `fusermount3` on PATH (`apt install fuse3`).
- `git` on PATH — the gateway spawns it for fetch/push, and the workflow is
  stock Git.
- Network access to whatever you clone. A local `file:///path/to/bare.git`
  works too and is the fastest way to try the write path.

## Start the server

```sh
tar xzf gfs-bundle.tar.gz
cd gfs-bundle
./start-server.sh
```

Leave it running; Ctrl+C stops it and unmounts every workspace it created.
Its lab directory is `~/.gfs-lab` (override with `GFS_LAB`; keep the path
short — a workspace's control socket lives beside it and Unix socket paths
cap at 108 bytes).

## The whole loop (second terminal)

```sh
export PATH="$PWD/gfs-bundle/bin:$PATH"   # from wherever you unpacked
cd ~/.gfs-lab

gfs clone https://github.com/pallets/flask.git
cd flask
export PATH="$(gfs install-shim):$PATH"

git status
git log --oneline -10
git switch -c my-change
echo "a change" >> README.md
git commit -am "a change"
git push                # lands at refs/gfs/work/dev/my-change on the gateway
```

Apart from the clone and the shim install, that is stock Git. `gfs push
my-change` would continue outward to the real upstream, but needs a
credential with write access (`GFS_UPSTREAM_CREDENTIAL`) — skip it for a
repository you cannot write to, or clone a local bare repo instead:

```sh
git clone --bare https://github.com/pallets/flask.git ~/.gfs-lab/mine.git
gfs clone file://$HOME/.gfs-lab/mine.git
# ...same loop... then the outward push works with no credential:
gfs push my-change
```

## What to look at while you do it

**The hydration line is the product.** Check it after anything:

```sh
gfs inspect | grep hydration      # 0 blobs after clone, log, status, commit
```

`git log`, `git status`, `git ls-files` all answer with **0 blobs
hydrated** — history and metadata come through the object-store projection
(the `odb` line in `gfs status` moves instead; commits and trees are small).
Reading a file hydrates exactly that file. Compare:

```sh
gfs rg -F 'ImproperlyConfigured'  # server-side: milliseconds, downloads nothing
rg -F 'ImproperlyConfigured'      # sweeps the tree: hydrates everything it reads
gfs inspect | grep hydration      # now look what the second one cost
```

**The shims route cost, not correctness.** With the shim PATH installed:

```sh
git gc                        # refused, exit 2 (walks the whole object database)
git blame src/flask/app.py    # runs, after a stderr note naming `gfs blame`
rg 'pattern'                  # runs, after a stderr note naming `gfs rg`
```

**The push landed where it should:**

```sh
git -C ~/.gfs-lab/repos/*.git for-each-ref refs/gfs/work
```

Work lives under `refs/gfs/work/dev/`, never `refs/heads/` — the branch
namespace mirrors upstream and is written only by fetch.

**Server-side review, zero download:**

```sh
gfs show HEAD~2 --stat
gfs diff HEAD~3..HEAD
gfs blame src/flask/cli.py -L 40,80
```

## Teardown

Ctrl+C in the server terminal does everything: unmounts the lab's
workspaces, stops the `gfs-fuse` host if it has nothing left to serve, and
stops the gateway. By hand: `gfs unmount --workspace <path>` for one
workspace, `gfs daemon stop` for everything. To start from nothing,
`rm -rf ~/.gfs-lab` — but only after the server has stopped; `rm -rf` over
a live mount fails slowly on purpose (the base is read-only).

## If something looks wrong

- **`gfs daemon status`** says whether a host is running and what it serves.
  The host log is at `$XDG_RUNTIME_DIR/gfs/host.log`.
- **A host survives server restarts on purpose.** If you re-copy new
  binaries, `gfs daemon stop` first — a running host is still the old code.
- **`core.autocrlf=true`** in your global git config corrupts checked-out
  shell scripts and makes `git status` lie about modifications. Set
  `git config --global core.autocrlf false` before testing on a fresh
  machine.
- **First clone of a big repository is the import**, tens of seconds for
  hundreds of thousands of refs; every later clone of the same URL is a
  sync and mounts in well under a second.
- **Mount errors after a hard kill** (`File exists`, `ENOTCONN`): stale FUSE
  mounts from a killed host. `fusermount3 -uz <path>` on the workspace and
  its `.gfs/odb`, or just re-run — current binaries sweep dead mounts
  themselves.
