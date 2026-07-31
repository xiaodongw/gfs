# m05c: what does `.git` itself cost through FUSE?

Date: 2026-07-31
Instrument: `spikes/git-projection` probe, extended with a writable passthrough
mode (`--rw`) and kernel negative-dentry caching (`--negative-ttl`).
Driver: `spikes/git-projection/measure-gitdir.sh`.

## Question

The single-mount workspace proposal (ADR 0011) overmounts the workspace
directory and serves the real `.git` through the same FUSE filesystem as the
tree: state lives *inside* the folder, shadowed by the mount, reached by the
daemon through retained directory handles. Every index read, lockfile, ref
update, and loose-object write then becomes FUSE traffic that today goes
straight to local disk. m05b measured Git's read demand on a projected object
store; this spike closes the remaining gap: **Git's demand on its own
directory, write path included.**

## Method

Two arms, identical in everything but where `GIT_DIR` points:

- **local** — the real git directory on disk (today's layout);
- **fuse** — the same directory behind a writable FUSE passthrough
  (attribute TTL 1 s and 60 s measured separately, then 60 s plus a 60 s
  negative-dentry TTL).

Both arms use the m05b staging: the same on-disk working tree, the same
shipped index, and `objects/info/alternates` pointing at the staged object
store on local disk — so pack traffic (m05b's already-measured cost,
unchanged by this proposal) drops out of the comparison. Each command runs
twice; the warm repeat is reported. WSL2, kernel 6.18.

## Results (warm wall-clock, ms)

linux (index 10.2 MB):

| command | local | fuse ttl1 | fuse ttl60 | fuse + negative |
|---|---|---|---|---|
| `git rev-parse HEAD` | 1 | 3 | 3 | 2 |
| `git status` | 111 | 119 | 120 | 118 |
| `git log --oneline -1000` | 55 | 131 | 130 | 53 |
| `git read-tree HEAD` | 138 | 944 | 938 | 145 |
| `git commit --allow-empty` | 55 | 77 | 70 | 65 |
| `git update-ref` | 1 | 5 | 5 | 3 |

django (index 894 KB): the same shape — `read-tree` 28 → 441 → 31 ms,
`log -1000` 28 → 109 → 28 ms, `status` 33 → 38 → 35 ms.

## What the counters say

1. **Negative loose-object lookups are the entire story.** `read-tree` on
   linux issued **6,524 `lookup` calls that answered ENOENT** — Git probes its
   primary `objects/??/` directories before falling through to packs and
   alternates, and through FUSE each probe is a round trip (~60 µs here,
   ~2 µs on ext4). That, not data volume, is the 6× slowdown; `log` pays the
   same tax per first-seen commit.
2. **Writes are cheap.** The 10.2 MB linux index rewrite is 79 write calls and
   costs ~7 ms of delta (138 → 145 ms). Lockfile create/rename/unlink churn
   (15 mutations per commit) costs single-digit ms.
3. **The mitigation is a kernel feature, not new machinery.** Replying to an
   ENOENT lookup with a node-id-0 entry and a TTL plants a **negative
   dentry**: repeated probes for the same absent name never leave the kernel,
   across processes. With a 60 s negative TTL, every command lands within a
   few ms of local disk (worst case `status` +6%, `commit` +10 ms). It is safe
   under a single mutator because the kernel itself drops a negative dentry
   when a name is created through the mount. External mutation of the shadowed
   state dir is the one hazard, and the layout makes it impossible by
   construction: the state is only reachable through the daemon.
4. **Attribute TTL is not the lever.** 1 s vs 60 s made no measurable
   difference in any arm; the probes are first-touch lookups, which attribute
   caching does not absorb.

One artifact worth recording: the *first* `status`/`commit` in each staged arm
costs seconds because `cp -a` gives every file a new inode and the shipped
index's stat data goes stale, forcing a full re-stat and index rewrite. GFS
proper ships indexes with `core.checkStat=minimal` (ADR 0009), which ignores
inode and device, so this cost is the instrument's, not the design's.

## Conclusion

Serving the real `.git` through the workspace mount is **at or near parity
with local disk** provided the filesystem plants negative dentries for absent
object probes. Without that one behavior it is 3–6× slower on object-heavy
commands and the proposal would be rejected on measurement; with it, the
single-mount layout has no measured performance objection. The remaining
engineering (union pack/info directories, recovery ordering, copy semantics)
is recorded in ADR 0011.
