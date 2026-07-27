# M2 — Read-only FUSE client

## Summary

M1 delivered a server that can resolve a revision, page a million-entry snapshot,
serve blobs, and keep a pinned commit alive under a heartbeat-renewed retention
lease. Nothing mounts it yet. M2 turns that API into a workspace: a lazy,
read-only FUSE mount of one pinned commit, with a verified shared blob cache, the
synthesized `.git` surface ADR 0005 selected, and the mount lifecycle that
PLAN.md M2.1 specifies.

The constraints that are already decided and must not be re-litigated here:

* **ADR 0003** — the daemon runs unprivileged on the host and publishes the mount
  to the job; a FUSE callback must never block, and the event loop must have more
  than one thread. Both remedies, not either. The CSI leg is deliberately
  unmeasured, and the amendment requires M2 to keep mount *publication* behind one
  replaceable seam and to mount and read as the same UID in its own tests.
* **ADR 0005** — the `.git` surface is synthesized and has **six** entries, not
  the four DESIGN.md listed: `HEAD`, `packed-refs`, `config`, `xvfs.json`,
  `objects/`, `refs/`. Without the two directories Git does not recognize the
  directory as a repository at all. The `git` shim is a correctness requirement,
  because `ls-files` and `diff` against the raw surface exit 0 with empty output.
* **ADR 0006** — base entries report the stored sanitized `snapshot_time`, never
  the raw committer time; `statfs` reports the overlay quota, not the host
  filesystem; the blob cache is keyed by `(repository_id, algorithm, oid)`,
  content-verified before publication, and scoped to one repository.

M3 owns the writable overlay, so everything in M2 is read-only: `EROFS` on every
mutation, an empty `xvfs status`, and an empty `git diff` — all of which are the
*correct* answers for a read-only mount, and all of which M3 rewires to the
journal.

## Plan

Four commits, each one green under `scripts/check.sh` before the next starts.

### 1. Filesystem core (`xvfs-fuse`)

* `client.rs` — the snapshot API client: gRPC for metadata, HTTP for blob bytes,
  carrying the bearer token and the mount capability on every call.
* `inode.rs` — base inode table. Sequential allocation memoized per path, so an
  inode is stable for the life of one mount generation and deliberately not
  across generations. Lookup counts, `forget`/`batch_forget`, and an open-handle
  pin so an inode cannot be dropped from under a live descriptor.
* `attr.rs` — `TreeEntryInfo` → `FileAttr`, with `snapshot_time` for all three
  timestamps and the Git mode mapping from DESIGN.md section 8.2, including the
  gitlink-as-empty-directory rule.
* `cache.rs` — the blob cache: single-flight per OID, download to a temporary
  file, verify `blob <size>\0<content>` against the repository hash algorithm,
  atomic rename, quota LRU that never evicts an open entry.
* `gitdir.rs` — the six-entry synthesized `.git`, served from memory and excluded
  from hydration accounting.
* `fs.rs` — the `fuser::Filesystem` implementation. Every callback that can touch
  the network hands its reply to a tokio task and returns immediately.

### 2. Mount lifecycle (`xvfsd`, `xvfs`)

* `xvfsd` — the client daemon. `CreateMount` first, then `mount.json`, then the
  FUSE session, then publication. Heartbeat renewal on the server-supplied
  interval, with the ADR 0006 alert threshold; signal handling and forced
  teardown; a 0600 control socket carrying `inspect`, `health`, `refresh`, and
  `unmount`.
* `publish.rs` — the seam ADR 0003's amendment demands. One trait, one local
  implementation (an atomically replaced symlink), and the bind-mount/CSI
  implementations left for M6.1/M7.4 to add without touching the filesystem code.
* `xvfs mount|unmount|inspect|health|refresh` in the CLI, talking to the daemon.

Landed as planned, with these additions discovered during the work: `state.rs`
(`mount.json`), `control.rs` (the socket protocol), `lease.rs` (heartbeat health),
`xvfs install-shim`, a `future` fixture carrying a 2050-dated commit, and a
separate `tests/exit_criteria.rs` holding the six measured criteria. The shim
binary lives in `xvfs-fuse` rather than in `xvfs-cli`, because it reads only the
mount and the synthesized surface.

### 3. Compatibility (`xvfs-test`, `xvfs-git-shim`)

* A raw-tree materializer oracle built from `git ls-tree` and `git cat-file`, and
  a full-tree comparison against the mount.
* The same comparison against a real `git checkout` of the `attrs` fixture, whose
  divergence is recorded as documented expected behaviour rather than a failure.
* POSIX behaviour subset, huge directories, deep paths, non-UTF-8 names, symlink
  loops, server loss, corrupt blobs, and cache eviction.
* The ADR 0005 shim grammar, and confirmation that everything outside it fails
  with an actionable message.

### 4. Exit criteria, report, and plan updates

Measure each of M2's six exit criteria, write `docs/reports/m2-completion.md`,
update PLAN.md, and extend `scripts/dev-stack.sh` and `scripts/check.sh` so a
mount is part of the one-command stack and the gate.

Measured: cold mount to a usable root in **99 ms** having downloaded **zero**
blob bytes; a two-file read from a 16 MiB snapshot transfers 28 bytes across two
blobs and four metadata requests; 1000 repeated `stat(2)` calls cost no round
trips; 50 misses on one path cost at most two; eight concurrent opens of a 12 MiB
blob cause one download; a warm cache survives an unclean daemon exit with zero
bytes re-fetched and zero verification failures.

Also amended ADR 0003 (its deferral trigger has fired: the prototype mounts
locally) and ADR 0005 (the shim landed in M2 rather than M3.3, and M3.3's scope
is correspondingly reduced to rewiring `status`/`diff`/`ls-files` to the journal).

## Decisions

### Inode stability is bought with two maps, not one

DESIGN.md section 8.2 wants two things that pull against each other: an inode
number stable for the life of the mount, and a table that does not grow without
bound when something walks a monorepo.

`by_path` (path → number) is never pruned; `records` (number → metadata) is
pruned when the kernel's lookup count and the open-handle count both reach zero.
A path that is looked up, forgotten, and looked up again therefore gets the same
number, while the heavier metadata is reclaimed. Pruning both maps would let a
re-looked-up path change identity mid-job, which is exactly the stale-`(device,
inode)`-cache hazard the design calls out; pruning neither would retain every
entry of a full tree walk.

Because a number is never reused for a different path, the FUSE generation is
always zero.

### Negative lookups are cached in the kernel, not in the daemon

A miss replies with an entry whose inode is zero and a long TTL — the low-level
FUSE convention for a cached negative — rather than with `ENOENT`. Against an
immutable commit this is exactly as correct as `ENOENT` and costs one upcall
instead of thousands: a compiler searching an include path produces a negative
lookup per candidate directory per header. Measured in the suite: fifty
`stat(2)` calls on a missing path cost at most two round trips.

### The blob ticket is minted at `open`, not at `lookup`

A ticket is authorization state with a five-minute server-side expiry. Attaching
one to every metadata lookup would usually mint a credential that expires
unused, and would put a credential in every `readdirplus` page. Minting at
`open` also means a **warm** open costs no RPC at all, because a cached blob
needs no ticket — which is what makes reopening a header free.

### `.git` is the first child of the root, not the last

The root's listing is `.git` at offset 2, then the base entries. Appending it
last would require exhausting the base listing before the first entry could be
emitted, which for a huge directory is the whole page set before `readdir`
returns anything.

### The daemon's own UID, and no `allow_other` by default

ADR 0003's amendment is explicit that M2's tests mount and read as the same UID,
because `user_allow_other` is a privileged host action and requiring it would
make `cargo test` need one. `MountConfig::allow_other` exists and defaults to
false; the daemon opts in when the deployment has been prepared.

### Publication is a symlink swap, not a bind mount

ADR 0003's amendment asks only that publication stay behind one replaceable
step. The local implementation is a symlink replaced by `rename(2)` because
`mount --bind` needs `CAP_SYS_ADMIN`, and the ADR's entire argument is that the
daemon needs no capability where it runs. The symlink gives exactly the property
`xvfs refresh` needs: the swap is atomic, a path resolved after it reaches the
new generation, and a descriptor opened before it keeps the old one.

`MountPublisher` has one implementation today. The bind-mount and CSI forms
replace it without touching the filesystem code, which is the deferral's price.

### `mount.json` holds the capability, at 0600

A restarted daemon cannot renew a lease it cannot prove it holds, and asking the
control plane to re-issue one would make lease renewal depend on the very
authentication round trip a control-plane outage removes. The file is 0600 in a
0700 directory the daemon owns — the same boundary ADR 0006 puts around the blob
cache. `SnapshotClient::capability_for_persistence` is deliberately verbose so
the one call that takes a credential out of the client reads as a decision.

### One mount per daemon

A daemon owning several mounts would have to define what a partial failure means
— one mount lost, the others alive — and ADR 0006's failure policy has no answer
for that. One process per mount makes "the daemon died" and "the mount is gone"
the same event.

### The `lease` subcommand, split out of `mount`

M1's `xvfs mount` created a lease and nothing else. M2's `xvfs mount` mounts, so
the lease-only path moved to `xvfs lease create|renew|release` rather than being
deleted: `scripts/dev-stack.sh` uses it to demonstrate M1's lease machine
without a filesystem, and an orchestrator debugging a lease should not have to
mount to do it.

### The shim answers from the mount, never from the server

`log -1` needs one commit's metadata, and DESIGN.md section 8.6 says `GetCommit`
supplies it. The daemon calls `GetCommit` once at mount time and embeds the
result in `.git/xvfs.json`, so the shim needs no network and — more importantly
— no credential. A shim that called the server would have to carry the mount
capability, and putting a credential in a `PATH`-installed wrapper that any
process in the job can invoke is a worse trade than one JSON read.

That also decides where the binary lives: `xvfs-fuse`, not `xvfs-cli`, because
it touches only the mount and the synthesized surface.

### The shim refuses outside an XVFS workspace

Installed early in `PATH`, it is invoked everywhere. Answering for an ordinary
Git repository would replace a working `git` with a crippled one, so it walks up
looking for `.git/xvfs.json` specifically and fails with "not an XVFS workspace"
when it finds none.

### XVFS does not enable `FUSE_WRITEBACK_CACHE`

Added after the milestone, closing PLAN.md M2.3's open mmap bullet. Measured in
`tests/mmap.rs`: writable `MAP_SHARED` works on FUSE **without** the writeback
cache, so the capability buys nothing we need — and enabling it would hand `size`
and `mtime` to the kernel, which ADR 0006's overlay logical clock cannot allow,
because the daemon has to assign `mtime` for an edit to be provably newer than
the base under host clock skew.

The write side needed its own one-file probe filesystem: XVFS refuses a
read-write `open`, so a writable mapping fails before `mmap` is reached and
measuring against the real mount would only re-measure that it is read-only.

### Refusals exit 2, not 1

`git diff --quiet` uses exit 1 to mean "there were differences". A refusal that
exited 1 would be read by a script as a successful non-empty diff.

## Details

- **`unsafe` appears twice, both with a recorded reason.** The workspace denies
  rather than forbids `unsafe_code` precisely for this. `Ownership::current`
  calls `geteuid`/`getegid`, the two POSIX calls specified never to fail and
  with no safe wrapper in `std`; reconstructing them from `/proc/self` would
  read the *real* UID, which is the wrong value. The `statfs` test calls
  `statvfs` for the same reason.
- **A real bug the suite caught.** Cache adoption after a restart keyed the
  index by the on-disk file name, which is only the tail of the digest. A
  restarted daemon would have re-downloaded its entire warm cache while
  reporting it as present. `a_partial_file_left_by_a_crash_is_discarded_on_adoption`
  is the test that found it.
- **The `content` fixture's `large-blob.bin` is 12 MiB**, which is what makes
  the single-flight test meaningful: eight concurrent openers reliably overlap.
- **A backgrounded daemon must not inherit stderr.** `xvfs mount` redirects the
  daemon's stderr to `<state-dir>/xvfsd.log`. Without it a daemon holds the
  write end of the caller's pipe open, so `xvfs mount | tee` never sees EOF and
  appears to hang long after the command finished. This was found by the dev
  stack hanging for ten minutes with no output.
- **Paths are made absolute inside `Daemon::start`.** A symlink target resolves
  relative to the *link's* directory, so a relative `--state-dir` published a
  workspace pointing at a path that does not exist. Found by the dev stack.
- **`git rev-parse --show-toplevel` reports the generation path, not the
  workspace path.** Git resolves the publication symlink, so a tool inside the
  workspace sees `<state-dir>/generations/N`. Correct, and worth knowing: a bind
  mount would report the workspace path instead, so the two publishers differ
  observably here. M6.1 owns whether that matters to the pilot's tooling.
- **A second bug the oracle caught, in the oracle.** `xvfs_test::git_raw`
  returns `String::from_utf8_lossy` of stdout, so reading `git ls-tree -z`
  through it mangled the two non-UTF-8 fixture names into U+FFFD — and the
  *mount*, which had the bytes right, looked wrong. `git_bytes` is the
  byte-exact form and the oracle uses it; `git_raw` keeps a warning in its docs.
- **`pjdfstest` and `xfstests` were not run.** Neither is installed here and
  neither is packaged as a Rust dependency. `compat.rs` covers a hand-written
  subset of the same ground — errno for the wrong object kind, `NAME_MAX`,
  reads past EOF, permission enforcement — and says in its own module docs that
  it is a subset rather than the suites. This is a real gap in M2.4's first
  bullet and is recorded as one in the report.
- **`rm -rf` over a live mount is a trap.** The dev stack originally cleaned up
  with it and spent minutes failing against a read-only base left by an earlier
  run. The script now unmounts first, then `fusermount3 -u -z` each generation,
  and only then removes anything.
