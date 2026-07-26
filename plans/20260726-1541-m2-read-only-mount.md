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
- **`rm -rf` over a live mount is a trap.** The dev stack originally cleaned up
  with it and spent minutes failing against a read-only base left by an earlier
  run. The script now unmounts first, then `fusermount3 -u -z` each generation,
  and only then removes anything.
