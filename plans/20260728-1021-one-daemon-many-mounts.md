# One Daemon, Many Mounts

## Summary

Today every `gfs clone` forks a `gfs-fuse` process that owns exactly one mount.
Two clones means two daemons, two gRPC channels, two blob caches, and two sets of
FUSE threads. This change makes one `gfs-fuse` process host many mounts: `gfs
clone` and `gfs mount` connect to a shared host daemon over a new host control
socket and ask it to create a mount, starting the daemon on demand if none is
running.

The socket topology the CLI depends on is **unchanged**. Each mount still binds
its own `<workspace>.gfs/control.sock`, so `workspace::discover`'s upward walk,
all fifteen `Request` variants, every CLI call site, and `gfs-git-shim` keep
working verbatim. One process can bind N sockets; the per-mount socket was never
an OS requirement, only a name (`/dev/fuse` is the kernel channel, and it is an
fd inside the process). That is what makes this change cheap.

## Plan

### Phase 1 — `crates/gfs-mount`: `Mount` and `MountHost`

* `daemon.rs` → `mount.rs`; `Daemon` → `Mount`; `DaemonConfig` → `MountSpec`.
  "Daemon" now means the host process, so leaving the per-mount object called
  `Daemon` would make every comment in the crate ambiguous.
* Split `serve_control` into `bind_control()` (synchronous, returns the bound
  listener) and `serve_control(listener)`. The host binds before it replies to
  `CreateMount`, which closes a race that exists today: `gfs mount` writes the
  ready file and then calls `Inspect` on a socket a spawned task has not bound
  yet. The test harness polls `is_live` to paper over it.
* New `host.rs`: `MountHost` owns `HashMap<state_dir, MountEntry>`, a shared
  `BlobCache` registry keyed by `(cache_dir, repository_id)`, the singleton lock,
  and the host socket.
* New host protocol in `control.rs`: `HostRequest` / `HostResponse`, alongside
  the untouched per-mount `Request` / `Response`.

### Phase 2 — `gfs-fuse` becomes the host binary

Arguments become host-level (socket, endpoints, token, defaults). An optional
initial mount keeps `--foreground` useful for debugging. SIGTERM/SIGINT tear
down every mount.

### Phase 3 — `gfs-cli`

* `do_mount` stops forking a daemon: resolve the host socket, start the host if
  it is not live, send `CreateMount`, print the report it returns.
* New `gfs daemon status|stop`.
* `gfs unmount` is unchanged on the wire; the host deregisters the mount when its
  control task returns.

### Phase 4 — tests and docs

Harness moves to `MountHost`; new smoke test for two mounts in one host; ADR 0003
and 0006 amendments for the failure-isolation change; `docs/manual-test.md`.

## Decisions

**Keep one control socket per mount.** The alternative — a single
mount-addressed endpoint — would have meant a selector on all fifteen `Request`
variants, a new discovery rule to replace the upward walk `gfs rg` depends on,
and changes to every CLI call site and to `gfs-git-shim`. It buys nothing the
host does not already have. The per-mount socket was never an OS requirement:
`/dev/fuse` is the kernel channel and it is an fd, not a name. Recorded as
ADR 0008.

**`Daemon` renamed to `Mount`, `DaemonConfig` to `MountSpec`.** "Daemon" now
means the host process, so leaving the per-mount object called `Daemon` would
have made every comment in the crate ambiguous. Mechanical and compiler-checked.

**`flock` for the singleton, not a pid file.** Every staleness rule for a pid
file is wrong when the pid is reused; the kernel releases an `flock` however the
process dies. Uses the documented `#[allow(unsafe_code)]` opt-out already
established by `attr::Ownership::current`, rather than adding a dependency.

**The host lingers; no idle-exit.** An idle timeout races with a concurrent
`gfs clone`, and the hosted shape wants a long-lived supervised process.
`gfs daemon stop` is the explicit action, and `scripts/dev-server.sh` uses it
only when the host has no mounts left.

**`--foreground` gets a private host on a socket inside the state directory.**
Attaching to the shared host would neither show its logs in this terminal nor
stop it on Ctrl-C, so the flag would not mean what it says.

**Bind the mount's control socket before answering `CreateMount`.** This closed a
pre-existing race the test harness was polling around.

## Details

**Verified end to end**, not just by tests: two `gfs clone`s against a live
gateway produce one `gfs-fuse` process with two mounts; `gfs status` and `gfs rg`
work from inside each tree with no flags; `gfs unmount` on one leaves the other
serving; `gfs commit` works through the host.

**Two bugs found and fixed on the way.**

1. `HostResponse::Mounts(Vec<MountSummary>)` was a newtype variant wrapping a
   sequence, which serde cannot serialize under an internally tagged enum. It is
   now a struct variant.
2. Both serve loops dropped a response that failed to encode, so the caller saw
   only "the daemon closed the control connection without replying" — the
   transport blamed for a payload fault. They now send an error response. This
   was pre-existing in `serve_control`.

**One pre-existing bug confirmed, not introduced.** A shell standing in a
workspace gets `ENOENT` on its next command after `gfs switch`, `gfs commit`, or
`gfs refresh`, because the generation it was standing in is retired as soon as
its open handles reach zero. Reproduced identically on the pre-change binaries
built from `848d5b3` in a scratch worktree. Already documented in
`docs/manual-test.md` (`cd $(pwd)` recovers).

**Not done, deliberately.** Each mount still opens its own gRPC channel;
`SnapshotClient::connect` builds the channel internally, so sharing it means
threading a pre-built channel through. Worth doing separately.
