# Performance: what is measured, how to run it again, and the numbers today

This page is the runbook for the two benchmark suites and a record of what
they measured on 2026-09-03. Everything below is meant to be repeated on
another repository and another machine by editing one config line and
running the same commands; the tables then rebuild themselves from the raw
runs.

## What is measured

Two suites, both in `spikes/corpus/`, both against repositories named in
`spikes/corpus/corpus.conf`:

| suite | question | script | results |
| --- | --- | --- | --- |
| local mode against the alternatives | is a lazily mounted workspace over a clone a better `git worktree add`, and what does the server mount cost on top | `benchmark-local.sh` | [`../benchmarks/local-mode.md`](../benchmarks/local-mode.md) |
| the FUSE levers | what each kernel-side lever buys on the local mount, and how far from a native checkout the result is | `benchmark-levers.sh` + `merge-levers.py` | [`../benchmarks/fuse-levers.md`](../benchmarks/fuse-levers.md), raw runs in [`../benchmarks/fuse-levers/`](../benchmarks/fuse-levers/) |

Both run one task in a working tree and time each step with wall-clock
seconds, one run per configuration, a fresh mount and a fresh host per run.
The levers suite's task, in order: read 2 000 files through (the largest
top-level directory, capped), read them again, `rg -F TODO` over the tree
twice, read the largest blob twice, write 64 MiB in 4 KiB pieces with `dd`,
`cp -r` one source directory in from the clone, read both back, `git status`,
`git add -A` + commit; then a per-operation loop in Python (2 000 iterations
each of `open`+`close`, `open`+`read`+`close`, `stat`, and `pwrite` of 4 KiB
on a fresh file) on one base file and one overlay file. Steps are ordered so
that "cold" means the first touch since the mount.

## Prerequisites

- A release build: `cargo build --release --workspace`. The scripts refuse
  to run against a debug build.
- `rg` (ripgrep), `python3`, `bc`, `git` 2.4x+, FUSE 3 (`fusermount3`), and
  `/dev/fuse` writable by the user.
- Disk for the corpus: a bare mirror plus a non-bare clone per repository
  (vscode: 2.8 GiB clone; django: 0.9 GiB).
- For the passthrough columns, the kernel needs `CONFIG_FUSE_PASSTHROUGH`
  (6.9+; check `zcat /proc/config.gz | grep FUSE_PASSTHROUGH`) and the
  daemon binary needs the capability:

  ```sh
  sudo setcap cap_sys_admin+ep target/release/gfs-fuse
  getcap target/release/gfs-fuse    # cap_sys_admin=ep
  ```

  Every `cargo build --release` that rewrites the binary drops it; set it
  again before a passthrough run. Without it the daemon logs that it lacks
  the capability and takes the unprivileged path, and `gfs inspect` shows
  `kernel 0 opens served by passthrough` — that is how to tell which column
  you actually measured.

## Adding a repository

1. Add a line to `spikes/corpus/corpus.conf`: `<id> <role> <url>`. The id is
   what every script takes on its command line.
2. Fetch the mirror: `./spikes/corpus/fetch-corpus.sh <id>` (bare, under
   `~/gfs-corpus/mirrors/<id>.git`; `GFS_CORPUS_DIR` moves the root).
3. Make the non-bare clone the local-mode suites mount from by running the
   local-mode suite once: `./spikes/corpus/benchmark-local.sh <id>`. It
   clones `~/gfs-corpus/local/<id>` from the mirror with LFS smudging
   disabled (the mirror has no LFS endpoint) and `gc.auto` off, and reuses
   it afterwards. `GFS_LOCAL_DIR` moves it.

A private monorepo works the same way: a `file://` or SSH URL in
`corpus.conf`, and nothing in the scripts names a repository.

## Recording the machine

Every results page states the machine. Capture it with:

```sh
uname -r; nproc; free -g | awk '/Mem/ {print $2 " GiB"}'
git --version; rustc --version
cat /sys/block/$(lsblk -no PKNAME "$(df --output=source ~/gfs-corpus | tail -1)")/queue/rotational  # 0 = SSD
```

and the per-round-trip floor that anchors every per-file number: `fuser`'s
hello filesystem does `open`+`close` in 48 µs on the 2026-09-03 machine
(WSL2, Linux 6.18, 32 cores, 46 GiB, Git 2.53, Rust 1.97); a different
kernel or virtualization layer moves everything by that ratio.

## Running the levers suite

One column per invocation. The label is the column header; the environment
chooses the configuration:

```sh
B=./spikes/corpus/benchmark-levers.sh
GFS_BENCH_NATIVE=1                       $B "native worktree" vscode django
                                         $B "baseline"        vscode django   # whatever is built
GFS_BENCH_MOUNT_FLAGS=--writeback-cache  $B "+ writeback cache" vscode django
GFS_BENCH_WAIT_PREWARM=1 GFS_BENCH_MOUNT_FLAGS=--prewarm $B "+ prewarm" vscode django
# after setcap:
                                         $B "passthrough" vscode django
GFS_BENCH_WAIT_PREWARM=1 GFS_BENCH_MOUNT_FLAGS=--prewarm $B "passthrough + prewarm" vscode django
```

Each run prints one `## <repo> — <label>` section per repository with a
two-column table and the `gfs inspect` lines that prove the configuration
(`kernel N opens served by passthrough`, `prewarm done, N blobs …`). Save
each run's output as a file under `benchmarks/fuse-levers/` — the prefix
orders the columns — and rebuild the tables:

```sh
$B "my label" myrepo > benchmarks/fuse-levers/06-my-label.md
./spikes/corpus/merge-levers.py benchmarks/fuse-levers/*.md
```

The merge output is what sits between the `<!-- merged tables -->` markers
in `benchmarks/fuse-levers.md`. Other knobs: `GFS_LEVERS_WORK` (scratch
root, default `~/gfs-corpus/levers-bench`), `GFS_RG_BINARY`.

Do not rebuild `target/release` while a run is in progress: the host for the
second repository is spawned from the binary on disk at that moment.

## Running the local-mode suite

```sh
./spikes/corpus/benchmark-local.sh vscode django
```

Three legs per repository — `git worktree add`, `gfs mount --local`, and a
`gfs-server` started over a copy of the mirror with `gfs mount --repo` —
running the same task, plus disk allocated per workspace and a check that
all three commits produced the same tree. It starts its own server on
`127.0.0.1:8630/8631` (`GFS_BENCH_HTTP_ADDR`, `GFS_BENCH_GRPC_ADDR`) and
its own host socket, so it never touches a developer's session.

## Per-operation measurement by hand

When a step moves and the reason is not obvious, measure the operation
alone. The recipe that found every result in
[ADR 0014](adr/0014-answer-on-the-fuse-thread-and-let-the-kernel-keep-it.md):

- a Python loop of 2 000 iterations of the one syscall pair, on a file in a
  mount that is already warm, reporting µs per iteration (the tail of
  `benchmark-levers.sh` is exactly this);
- the daemon's CPU per thread from `/proc/<pid>/task/*/schedstat` before and
  after, which separates "the daemon worked" from "the daemon waited";
- the kernel's request stream: start `gfs-fuse` by hand with
  `RUST_LOG=info,fuser=debug` and count request kinds between two marks —
  the CLI-spawned daemon's log does not carry it.

Traps: `ptrace` attach to the daemon is refused (`yama=1`); a daemon started
under `strace` cannot mount (setuid `fusermount3`); a shell command that
times out kills the daemon it spawned and leaks `refs/gfs/mounts/*` anchors
in the clone — delete them with `git update-ref -d`.

## The numbers on 2026-09-03

Full tables with every column are in the two results pages; this is the
shape of the answer. vscode is 17 926 files in a 2.8 GiB clone; django is
7 078 files in 871 MiB.

**Creating a workspace** (from `local-mode.md`):

| | git worktree | gfs local | gfs server |
| --- | ---: | ---: | ---: |
| one vscode workspace | 2.31 s, 298 MiB | 0.17 s, 2.8 MiB | 0.23 s, 3.2 MiB + 62 MiB cache |
| five vscode workspaces | 12.0 s, 1 490 MiB | 0.69 s, 12.8 MiB | — |

**Using it** (vscode, from `fuse-levers.md`; "best" is passthrough +
prewarm):

| step | native worktree | gfs, ADR 0014 | gfs, best | best ÷ native |
| --- | ---: | ---: | ---: | ---: |
| read 2 000 files, first time | 0.041 s | 0.746 s | 0.452 s | 11× |
| read again | 0.040 s | 0.264 s | 0.273 s | 7× |
| `rg` over the tree, first | 0.041 s | 0.654 s | 0.413 s | 10× |
| `rg` again | 0.034 s | 0.158 s | 0.182 s | 5× |
| 64 MiB in 4 KiB writes | 0.070 s | 3.950 s | 0.894 s | 13× |
| `cp -r` 10 225 files in | 1.10 s | 29.4 s | 26.1 s | 24× |
| read those files back | 0.20 s | 2.77 s | 1.80 s | 9× |
| `git status` | 0.55 s | 0.73 s | 0.76 s | 1.4× |
| `git add -A` + commit | 1.14 s | 4.92 s | 3.78 s | 3.3× |
| `open`+`close`, one file | 2.7 µs | 63.5 µs | 65 µs | 24× |
| one 4 KiB write | 3.0 µs | 244 µs | 52 µs | 17× |

**What each lever bought**, in isolation, on vscode:

| lever | needs | moved | did not move |
| --- | --- | --- | --- |
| passthrough (ADR 0015) | `CAP_SYS_ADMIN` on `gfs-fuse` | 4 KiB write 244 → 52 µs; overlay reopen+read 216 → 87 µs; 64 MiB read-back 48 → 9 ms; commit 4.9 → 3.9 s | warm base-blob reads (already page-cache hits); `cp -r` |
| `--writeback-cache` (ADR 0016) | nothing; opt-in | 4 KiB write 244 → 52 µs; the `dd` 3.95 → 0.85 s | everything else; a refused write is reported at `close` |
| `--prewarm` | nothing; local mode | cold read of 2 000 files 0.75 → 0.49 s; first `rg` 0.65 → 0.40 s, for 0.38 s of background inflate | warm anything |

Passthrough and the writeback cache do not combine: the kernel grants
neither when asked for both, and the daemon drops the writeback request
when it has passthrough.

**Where the remaining time is.** Per file read: two kernel round trips
(`open`, `release`) at the machine's 16–25 µs floor, which is the 10× on
whole-tree reads. Per file written by a tool like `cp`: about 2.6 ms, none
of it in `write` — the journal commit on each `create` and `release`. That
is the next lever and it lives in the overlay, not in FUSE.

## Reading a result

- Columns are single runs; treat differences under about 10 % on the
  sub-second steps as noise, and re-run before concluding anything from
  them. The per-operation rows are stable to a few µs.
- The `gfs inspect` lines under each table are the configuration's proof.
  Zero passthrough opens in a "passthrough" column means the capability
  was not on the binary that ran.
- `anchors left: 0` is the cleanup check; a non-zero count means a daemon
  was killed mid-run and the clone holds `refs/gfs/mounts/*` to delete.
