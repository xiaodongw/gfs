# M2 — Read-only FUSE client: completion report

Date: 2026-07-26
Milestone: M2 (PLAN.md section 5)
Status: **Complete**, with one recorded gap in M2.4 and one deferred item that is
not M2's to close.

M1 delivered an API that could resolve a revision, page a million-entry snapshot,
serve blobs, and keep a pinned commit alive under a heartbeat-renewed lease.
Nothing mounted it. M2 turns that into a workspace: a lazy read-only FUSE mount
of one pinned commit, a verified shared blob cache, the synthesized `.git`
surface ADR 0005 selected, the `git` shim that surface requires for correctness,
and the mount lifecycle around all of it.

## The exit gate

PLAN.md section 5 states six criteria. Each is listed with what verifies it, so
the claim can be checked rather than taken.

| # | Criterion | Verified by | Result |
| --- | --- | --- | --- |
| 1 | Cold mount meets the startup/download target | `xvfs-fuse/tests/exit_criteria.rs::criterion_1_cold_mount_meets_the_startup_and_download_target` | **Met, measured** |
| 2 | Reading selected files transfers only required metadata and blobs | `exit_criteria.rs::criterion_2_...`, `mount.rs::reading_one_file_does_not_hydrate_its_siblings` | **Met, measured** |
| 3 | Representative read-only build/analysis tasks succeed, with repository-root probing working against the `.git` surface | `exit_criteria.rs::criterion_3_...`, `compat.rs::stock_git_finds_the_repository_root_through_the_synthesized_surface` | Met **within a bounded definition of "representative"** — see below |
| 4 | Base timestamps stable across remounts and hosts, including future-dated commits and clock skew | `exit_criteria.rs::criterion_4_a_future_dated_commit_reports_a_sane_base_timestamp`, `::criterion_4_base_timestamps_are_identical_across_remounts`, `xvfs-types::time` unit tests | Met |
| 5 | Refresh exposes only the old or new generation; open old-generation handles stay valid | `lifecycle.rs::refresh_swaps_generations_and_keeps_open_handles_on_the_old_one`, `publish.rs` unit tests | Met |
| 6 | Daemon or server failure does not corrupt the shared cache | `exit_criteria.rs::criterion_6_daemon_failure_does_not_corrupt_the_shared_cache`, `::criterion_6_server_failure_leaves_cached_content_readable` | Met |

### Measured numbers

Machine: the M0.1 profile (WSL2, Linux 6.18.33.2, 32 logical CPUs, 46 GiB RAM),
debug build, server and client in one process over loopback.

| Measurement | Result | Target |
| --- | ---: | ---: |
| Cold mount to a usable root (`bigdir`, 5002-entry tree) | **99 ms** | < 2 s |
| Blob bytes downloaded to reach a usable root | **0** | < 10 MiB |
| Selective read of 2 files from a 16 MiB, 7-file snapshot | **28 bytes, 2 blobs, 4 metadata requests** | only what is required |
| Repeated `stat(2)` on one path, ×1000 | **0** additional round trips | ADR 0003 measured 0 upcalls |
| 50 `stat(2)` calls on a *missing* path | **≤ 2** round trips | — |
| 8 concurrent opens of one 12 MiB blob | **1** download | — |
| Warm cache after an unclean daemon exit | **0 bytes re-fetched, 0 verification failures** | no corruption |

The zero in row two is the load-bearing one. The 10 MiB figure in ADR 0006 is a
ceiling; a mount reaches a usable root having downloaded no file content at all,
because nothing in the startup path needs any.

### Criterion 3, and what "representative" can honestly mean here

ADR 0006 records open question 2 — **which monorepos and agent workloads define
success** — as unresolved and needing product input. There is therefore no
pilot task corpus for M2 to run, and claiming one would be claiming more than
exists.

What is verified is the two shapes such a task reduces to, plus the probing
behaviour the criterion names:

- **a real compiler reads its input through the mount**: `rustc` builds
  `src/main.rs` from the mounted tree, writing its output outside;
- **an analysis tool sweeps it**: `grep -rl` over the mount finds exactly the
  expected file;
- **repository-root probing works**: stock `git rev-parse --show-toplevel`,
  `--git-dir`, `HEAD`, `--abbrev-ref HEAD`, and `symbolic-ref --short HEAD` all
  answer correctly against the synthesized surface, with no `safe.directory`
  configuration in the same-UID case.

The `grep -r` measurement also demonstrates the cost DESIGN.md section 8.4 warns
about rather than hiding it: sweeping the tree hydrated every blob in it. A
program that opens every file hydrates every file unless a budget stops it, and
M6.2 owns that budget.

## Findings that changed something

Four, in descending order of how much they mattered.

### 1. Cache adoption after a restart was keyed by the wrong string

The on-disk name of a cached blob is the *tail* of its digest — the first two
hex characters are the shard directory. Adoption inserted index entries keyed by
the file name alone, so every adopted entry was unfindable: a restarted daemon
would have re-downloaded its entire warm cache while reporting it as present on
disk. Found by
`cache::tests::a_partial_file_left_by_a_crash_is_discarded_on_adoption`, and it
is now the reason criterion 6 can assert **0 bytes re-fetched**.

### 2. The test oracle corrupted the very paths it was checking

`xvfs_test::git_raw` returns `String::from_utf8_lossy` of stdout. Reading
`git ls-tree -z` through it mangled the two non-UTF-8 fixture names into U+FFFD
— and the *mount*, which had the bytes exactly right, was reported as the thing
that was wrong. `git_bytes` is now the byte-exact form and the oracle uses it;
`git_raw` carries a warning in its own documentation.

This is the failure mode ADR 0006 predicted from the other direction: non-UTF-8
paths are absent from every corpus tip, so byte handling is insurance — and
insurance that is never exercised is not insurance.

### 3. A backgrounded daemon must not inherit stderr

`xvfs mount` starts `xvfsd` and returns. A daemon holding the caller's inherited
stderr keeps the write end of that pipe open, so `xvfs mount | tee` never sees
EOF and appears to hang long after the command finished — which is exactly what
the development stack did, for ten minutes, with no output at all. The daemon's
stderr now goes to `<state-dir>/xvfsd.log`.

### 4. Publication needs absolute paths

A symlink target resolves relative to the *link's* directory, not to the
daemon's working directory, so a relative `--state-dir` published a workspace
pointing at a path that does not exist. `Daemon::start` now makes the state
directory, workspace, and cache directory absolute before anything else.

## Decisions worth carrying forward

**Inode stability is bought with two maps.** The path-to-number map is never
pruned, so a number is stable across a forget/lookup cycle; the metadata behind
a live inode is dropped at zero references. Pruning both would let a
re-looked-up path change identity mid-job — the stale-`(device, inode)`-cache
hazard DESIGN.md section 8.2 calls out. Pruning neither would retain every entry
of a full tree walk, and a monorepo has millions.

**Negative lookups are cached by the kernel.** A miss replies with inode zero
and a long TTL rather than `ENOENT`. Against an immutable commit that is exactly
as correct and costs one upcall instead of thousands; a compiler searching an
include path produces a negative lookup per candidate directory per header.

**The blob ticket is minted at `open`, not at `lookup`.** A ticket is
authorization state with a five-minute expiry, so attaching one to every
metadata lookup would usually mint a credential that expires unused. Minting at
`open` also makes a *warm* open free: a cached blob needs no ticket and
therefore no round trip.

**Publication is one replaceable step.** ADR 0003's amendment asked for exactly
this in exchange for deferring the Kubernetes measurement. The local
implementation replaces a symlink with `rename(2)`, because `mount --bind` needs
`CAP_SYS_ADMIN` and the ADR's whole argument is that the daemon needs no
capability where it runs. The swap gives what refresh requires: a path resolved
after it reaches the new generation, and a descriptor opened before it keeps the
old one.

**The `git` shim needs no credential.** The daemon calls `GetCommit` once at
mount time and embeds the result in `.git/xvfs.json`, so the shim's bounded
`log -1` reads a local file. A shim that called the server would have to carry
the mount capability, and putting a credential in a `PATH`-installed wrapper any
process can invoke is a worse trade than one JSON read.

## Recorded gaps

### `pjdfstest` and `xfstests` were not run

PLAN.md M2.4's first bullet asks for a relevant subset of these suites.
**Neither was run.** Neither is installed in this environment and neither is
packaged as a Rust dependency. `xvfs-fuse/tests/compat.rs` covers a hand-written
subset of the same ground — `ENOTDIR` and `EISDIR` for the wrong object kind,
`ENAMETOOLONG`, reads past EOF, kernel-enforced permissions, the read-only
boundary — and says in its own module documentation that it is a subset rather
than the suites.

This is a real gap. It should close before M6.1, alongside the other deployment
work, and it is cheap: the suites are packaged for Debian and Ubuntu.

### The cross-UID bind-mount path is still unmeasured here

`user_allow_other` remains unset in `/etc/fuse.conf` on this host, so M2's tests
mount and read as the same UID — which is what ADR 0003's amendment specifies,
because requiring a privileged host action to run `cargo test` is a worse trade
than deferring one integration path. `MountConfig::allow_other` exists and
defaults to false.

**The deferral now has a trigger.** ADR 0003's amendment says the Kubernetes and
real-runner measurement waits until the prototype mounts and serves a workspace
locally. It does, as of this milestone. Re-running
`spikes/fuse-probe/deployment-matrix.sh` against the real hosted runner is
therefore unblocked, and it stops being deferrable before M6.1.

### One observable difference between publishers

`git rev-parse --show-toplevel` inside a workspace reports the **generation**
path, not the workspace path, because Git resolves the publication symlink. A
bind-mount publisher would report the workspace path instead, so the two differ
observably. Nothing in M2 depends on it; M6.1 owns whether the pilot's tooling
does.

## What M2 deliberately did not build

Everything writable. Every mutation is `EROFS` (`link` is `EPERM`, per DESIGN.md
section 8.2, because Git has no hard links to model), and the shim's `status`
and `diff` report a clean tree. Those are the *correct* answers for a read-only
mount of an immutable commit, not stubs — there is no overlay yet to differ from
the base. M3 rewires all three to the overlay journal.

The overlay quota is *reported* by `statfs` and not enforced, for the same
reason: there is nothing yet to write.

## Test inventory

| Suite | Cases | Covers |
| --- | ---: | --- |
| `xvfs-fuse` unit tests | 33 | inode lifetimes, cache eviction and pinning, attribute mapping, the `.git` surface, publication, lease health, the shim's pathspec matcher |
| `tests/mount.rs` | 25 | metadata and inode model, blob cache, modes and symlinks, non-UTF-8 paths, 5002-entry pagination, 40-level nesting, `EROFS`, `statfs`, server loss |
| `tests/lifecycle.rs` | 8 | ordering, `mount.json`, control socket, lease renewal and failure, refresh generations, double-start refusal |
| `tests/compat.rs` | 20 | raw-tree oracle over 7 fixtures, filtered-checkout divergence, POSIX subset, stock Git against the surface, the shim's frozen grammar |
| `tests/exit_criteria.rs` | 8 | the six criteria above |

`scripts/check.sh` now fails when `/dev/fuse` or `fusermount3` is missing rather
than letting the mount tests fail obscurely, and `scripts/dev-stack.sh`
demonstrates the mount, the `.git` surface, the shim contrast (stock Git: 0
tracked files; shim: 4), refresh, health, and unmount.
