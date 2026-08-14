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
# Keep Ubuntu's native Git ahead of Databricks' /usr/bin command shim.
export PATH="$PWD/gfs-bundle/bin:/usr/lib/git-core:$PATH"
cd ~/.gfs-lab

git clone https://github.com/pallets/flask.git
cd flask

git status
git log --oneline -10
find src -name '*.py' | head
grep -rn "def route" src | head
git switch -c my-change
echo "a change" >> README.md
git commit -am "a change"
git push -u origin my-change
```

The bundle's `bin/` is the pre-configured tool surface: `git`, `grep`,
`find`, and `rg` are the GFS shims, and they pass through to the real tools
whenever GFS cannot serve an invocation. `gfs push my-change` would continue
the gateway branch outward to the real upstream, but needs a
credential with write access (`GFS_UPSTREAM_CREDENTIAL`) — skip it for a
repository you cannot write to, or clone a local bare repo instead:

```sh
git clone --bare https://github.com/pallets/flask.git ~/.gfs-lab/mine.git
git clone file://$HOME/.gfs-lab/mine.git
# ...same loop, including `git push -u origin my-change`...
# Then the outward push works with no credential:
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
rg -F 'ImproperlyConfigured'                    # shim: server-side, no hydration
GFS_SHIM_BYPASS=1 rg -F 'ImproperlyConfigured'  # real rg: hydrates what it reads
gfs inspect | grep hydration                    # now look what the second one cost
```

**The shims route cost, not correctness.** The bundle PATH already has them:

```sh
git gc                        # refused, exit 2 (walks the whole object database)
git blame src/flask/app.py    # runs, after a stderr note naming `gfs blame`
find src -name '*.py'         # answered from the local index, no tree walk
grep -rn 'pattern' src        # translated to server-side `gfs rg`
rg 'pattern'                  # delegated to server-side `gfs rg`
```

**The push landed where it should:**

```sh
git -C ~/.gfs-lab/repos/<repo-id>.git show-ref refs/heads/my-change
```

The gateway is a shared fork: stock `git push` writes the branch you name
under `refs/heads/`. `gfs push my-change` is the separate step that
continues that gateway branch outward to the real upstream.

**Server-side review, zero download:**

```sh
gfs show HEAD~2 --stat
gfs diff HEAD~3..HEAD
gfs blame src/flask/cli.py -L 40,80
```

## Browse it over WebDAV

The same server speaks read-only WebDAV at `/dav/` — no `gfs`, no `git`,
no FUSE. Any repository the server holds is browsable at
`/dav/<repo-id>/<branch>/...`, where `<repo-id>` is what `git clone`
printed (e.g. `github.com_pallets_flask`). This server runs the no-token
dev posture, so no credentials anywhere:

```sh
curl -s -X PROPFIND -H 'Depth: 1' http://127.0.0.1:8430/dav/           # repos
curl -s -X PROPFIND -H 'Depth: 1' \
  http://127.0.0.1:8430/dav/<repo-id>/main/ | grep -o '<D:href>[^<]*'  # tree
curl -s http://127.0.0.1:8430/dav/<repo-id>/main/README.md             # bytes
```

A branch with a slash in its name is nested folders (`topic/deep` is
`topic/` containing `deep/`). Writes answer `405`; a `PROPFIND` without a
`Depth` header answers `403` on purpose (Depth infinity over a monorepo is
a tree walk nobody meant to request).

As a mounted volume:

- **macOS Finder**: Go → Connect to Server → `http://<host>:8430/dav`.
  Mounts read-only — the server advertises no LOCK support.
- **Windows Explorer**: `net start webclient` once (admin; error 67 means
  it is not running), then `net use Z: http://<host>:8430/dav` — or the
  redirector's native `net use Z: \\<host>@8430\dav`. In `cmd.exe`, the
  curl examples need double quotes: `-H "Depth: 1"` — single quotes are
  literal there, and the mangled header earns the `403`.

The server binds 127.0.0.1. From another machine, start it with
`GFS_HTTP_ADDR=0.0.0.0:8430 ./start-server.sh` — and remember anyone who
can reach the port is `dev`. A Windows host reaches a WSL2-hosted server
at `localhost` with no rebinding.

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
