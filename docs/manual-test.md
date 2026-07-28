# Manual testing: driving XVFS by hand

Every command here was run against a live Django workspace on 2026-07-27, and the
expected output is quoted so you can tell a pass from a failure without guessing.

For an automated smoke test instead, `scripts/dev-stack.sh --smoke` brings up the
same stack against small fixtures and exits non-zero if anything is broken. This
document is for poking at it yourself with a real repository.

## Prerequisites

- A release build: `scripts/build-release.sh`, or
  `cargo build --release -p xvfs-cli -p xvfs-fuse -p xvfs-server`.
- FUSE: `/dev/fuse` present and `fusermount3` on `PATH` (ADR 0003).
- A repository mirror. `./spikes/corpus/fetch-corpus.sh django` fetches one to
  `$HOME/xvfs-corpus/mirrors/django.git` (788 MiB). Any bare repository works;
  Django is used below because it is small enough to be quick and high-ref enough
  to be interesting.

## 0. Shell setup

Needed in **every** terminal you use, including a second one opened later.

```sh
cd /path/to/xvfs
export L=$HOME/xvfs-lab                       # the lab directory
export X=$PWD/target/release
export XVFS_ENDPOINT=http://127.0.0.1:8801
export XVFS_HTTP_ENDPOINT=http://127.0.0.1:8800
export XVFS_TOKEN=lab-token
```

**Keep `$L` short.** The daemon's control socket is `<workspace>.xvfs/control.sock`
and a Unix socket path cannot exceed 108 bytes. A workspace under a long
scratch path fails to serve, and the failure is easy to misread.

## 1. Start a server

```sh
rm -rf $L && mkdir -p $L
# Copied, never served in place: the server writes `refs/xvfs/*` lease anchors
# into whatever repository it serves, and a mirror is an input.
cp -a $HOME/xvfs-corpus/mirrors/django.git $L/repo.git

KEY=$(printf 'xvfs-manual-lab-key-not-for-production-use!!!!!!' \
  | od -An -tx1 | tr -d ' \n' | cut -c1-64)

$X/xvfsd-server --state-dir $L/server \
  --http-addr 127.0.0.1:8800 --grpc-addr 127.0.0.1:8801 \
  --capability-key "$KEY" --dev-token "$XVFS_TOKEN" \
  --import "django=$L/repo.git" > $L/server.log 2>&1 &
echo $! > $L/server.pid

until curl -fsS http://127.0.0.1:8800/readyz; do sleep 0.5; done
```

**The first start takes about 50 seconds**, almost all of it reconciling
Django's 29 298 refs. Restarting against the same `--state-dir` takes 0.2 s. Wait
for `readyz` rather than assuming the server is up — everything after this fails
confusingly if it is not.

## 2. Mount

```sh
$X/xvfs mount --repo django --rev HEAD --workspace $L/ws --cache-dir $L/cache
$X/xvfs inspect --workspace $L/ws
```

Expect the mount in well under a second and:

```
hydration  0 blobs, 0 bytes, 0 cache hits
overlay    0 changed paths (0 deleted), 0 of 1073741824 bytes used
```

**Watch the `hydration` line for the rest of this document.** It is the number
that says whether an operation actually avoided reading the repository, and it is
the one thing a plausible-looking result cannot fake.

## 3. The three server-answered tools

An agent asks three questions that want to touch every path. All three are
answered by the server; none of them reads the mount.

```sh
cd $L/ws
$X/xvfs-log -5 --oneline
$X/xvfs-find '*options.py'
$X/xvfs-rg -F 'ImproperlyConfigured' -m 5
$X/xvfs inspect --workspace $L/ws | grep hydration     # still 0 blobs, 0 bytes
```

`xvfs-log` prints five real commits and then, on stderr,
`more history follows; continue with --skip 5`. `xvfs-find '*options.py'` finds
3 files. `xvfs-rg` prints matches plus a coverage note on stderr about binary
files skipped — that note is deliberate, not a warning of failure.

### Glob syntax is gitignore's, and this is the part that surprises people

| Pattern | Matches |
| --- | --- |
| `?` | one byte that is not `/` |
| `*` | zero or more bytes, **none of them `/`** |
| `**` | zero or more path components, `/` included |

**A pattern with no `/` in it matches the file name, not the whole path.** So:

```sh
$X/xvfs-find '*options.py'            # 3   -- basename match
$X/xvfs-find '*/admin/options.py'     # 0   -- `*` cannot cross `/`
$X/xvfs-find '**/admin/options.py'    # 2   -- `**` can
```

### The comparison worth making yourself

```sh
rg -F 'ImproperlyConfigured' .                          # the wrong way
$X/xvfs inspect --workspace $L/ws | grep hydration      # look at what it cost
```

Real ripgrep gives the same answer in about 24 seconds and downloads the entire
tree — roughly 6 200 blobs and 47 MB. `xvfs-rg` answers in tens of milliseconds
and downloads nothing. That difference is the whole product.

## 4. The `git` shim

```sh
export PATH="$($X/xvfs install-shim --workspace $L/ws):$PATH"
git rev-parse HEAD          # works
git ls-files | wc -l        # 7077
git log -3 --oneline        # REFUSED, exit 2, prints the supported grammar
git rev-parse --short HEAD  # also refused: the grammar is frozen and narrow
```

The refusals are the designed behaviour. Without the shim, stock
`git ls-files` inside a mount exits **0 with empty output** — it reports that
nothing is tracked — which is why ADR 0005 calls the shim a correctness measure
rather than a convenience. Try it if you want to see it:

```sh
/usr/bin/git ls-files | wc -l   # 0 files, exit 0 -- reports nothing is tracked
```

For history and filenames, use `xvfs-log` and `xvfs-find`; the shim is not wired
to them yet.

## 5. Edit, status, diff, export

```sh
cd $L/ws
printf '\n# manual test\n' >> README.rst
echo 'notes' > NOTES.md
rm -f django/utils/lorem_ipsum.py

$X/xvfs status --workspace $L/ws
```

```
On refs/heads/main at sha1:c2517faff335f683e1cbe55d9844910b3fb40670
A NOTES.md
M README.rst
D django/utils/lorem_ipsum.py
```

Status comes from the overlay journal, not a tree scan, so it costs the size of
your edit set rather than the size of the repository. Search reflects the edits
immediately:

```sh
$X/xvfs-rg -F 'manual test'     # finds your new line in README.rst
$X/xvfs-find 'NOTES.md'         # finds the created file
$X/xvfs diff --workspace $L/ws | head
$X/xvfs export --workspace $L/ws --bundle $L/bundle
```

The bundle holds `manifest.json`, `changes.patch`, `content/` and `CHECKSUMS`.

## 6. Land the change server-side

There is no `git commit` in a workspace. The export is applied where the objects
are — with a temporary index and no worktree, which is what a server would do:

```sh
BASE=$(git --git-dir=$L/repo.git rev-parse HEAD)
GIT_INDEX_FILE=$L/idx git --git-dir=$L/repo.git read-tree $BASE
GIT_INDEX_FILE=$L/idx git --git-dir=$L/repo.git apply --cached \
  --whitespace=nowarn $L/bundle/changes.patch
TREE=$(GIT_INDEX_FILE=$L/idx git --git-dir=$L/repo.git write-tree)
CMT=$(git --git-dir=$L/repo.git -c user.name=lab -c user.email=lab@example.com \
  commit-tree $TREE -p $BASE -m "manual test")
git --git-dir=$L/repo.git update-ref refs/heads/manual $CMT
git --git-dir=$L/repo.git diff --stat $BASE manual
```

Expect exactly the three changes you made. This takes about 30 ms.

## 7. The smart-HTTP gateway

Stock Git, with no XVFS on the client at all:

```sh
git -c "http.extraHeader=Authorization: Bearer $XVFS_TOKEN" -c protocol.version=2 \
  clone --depth 1 http://127.0.0.1:8800/v1/repos/django $L/clone
git -C $L/clone fsck --no-progress && echo "fsck clean"
```

And the internal namespace must not be visible:

```sh
git -c "http.extraHeader=Authorization: Bearer $XVFS_TOKEN" \
  ls-remote http://127.0.0.1:8800/v1/repos/django | grep -c 'refs/xvfs/'   # 0
```

An unauthenticated clone gets `401`; a clone of a repository you cannot see gets
`404` rather than `403`, because a distinct status would answer the existence
question.

## 8. Two behaviours worth checking by hand

Both were defects found by `spikes/conformance/pjdfstest.sh` and fixed on
2026-07-27; see [`reports/posix-conformance.md`](reports/posix-conformance.md).

**A directory's timestamps advance when its contents change.** Build systems and
watchers rely on this.

```sh
cd $L/ws && mkdir -p dtest
a=$(stat -c %Y dtest); sleep 1.1; touch dtest/x; b=$(stat -c %Y dtest)
[ "$b" -gt "$a" ] && echo "advanced" || echo "INERT -- regression"
```

**A path too long for the filesystem is `ENAMETOOLONG`, not `EIO`.** The trick is
a path that is short relative to the current directory but long measured from the
workspace root — XVFS caps the latter at 4 096 bytes, while POSIX caps the
pathname handed to the syscall:

```sh
cd $L/ws && rm -rf deep && mkdir deep && cd deep
c=$(printf 'a%.0s' $(seq 1 250)); p=""
for i in $(seq 1 16); do p="$p$c/"; done
mkdir -p "${p%/}" && cd "${p%/}"          # now 4022 bytes from the workspace root
touch "$(printf 'b%.0s' $(seq 1 250))"    # File name too long
touch short                               # still works at the same depth
```

## 9. Teardown

```sh
$X/xvfs unmount --workspace $L/ws
kill $(cat $L/server.pid)
```

Unmount before deleting anything. `rm -rf` over a live mount is both wrong and
slow: the base is read-only so every removal fails, and ADR 0003 measured that a
mount point outlives its daemon and answers `ENOTCONN` until something unmounts
it.

## Traps

Each of these produced a plausible-looking wrong answer during development.

- **Long workspace paths fail the control socket.** See section 0. Under 108
  bytes for `<workspace>.xvfs/control.sock`.
- **Do not `pkill -f xvfsd`.** The pattern can match the shell running it. Kill
  by PID: `kill $(cat $L/server.pid)`.
- **`du` on a live mount walks the FUSE tree** and reports ~45 MiB for Django.
  The real client-side state is around 100 KiB, most of it the installed shim
  binary: `du -sb --exclude=generations $L/ws.xvfs`. A workspace with no shim
  installed and no edits is under 30 KiB.
- **`core.autocrlf=true`** in your global Git config makes anything you clone
  with shell scripts in it arrive with CRLF endings and fail in ways that look
  like the cloned project is broken. It also corrupts `git apply`.
- **Wait for `/readyz`.** A high-ref repository takes tens of seconds to import
  the first time.
- **A second terminal needs the section 0 exports again**, `PATH` shim included.
