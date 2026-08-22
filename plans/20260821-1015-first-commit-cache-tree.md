# The first commit in a workspace: ship a cache tree

## Summary

The agent-workflow benchmark records `commit` at **14.501 s** on vscode against
**1.002 s** for raw Git, and **9.705 s** against **0.054 s** on django. The gap
is not the projection, the network, or the overlay. It is one missing extension.

`crates/gfs-git/src/index.rs` wrote the seeded index deliberately with "no
extensions", which means no `TREE` — no cache tree. Without one Git cannot know
that any directory is unchanged, so the **first** `git commit` in a workspace
re-derives every tree in the repository and writes each one out:

| fresh mount, five files changed | django | vscode |
| --- | ---: | ---: |
| `git add -A` | 1.17 s | 1.56 s |
| `git commit` | **8.06 s** | **12.34 s** |
| loose objects written | 3 254 | 4 299 |
| …that already existed in the served pack | 3 240 | 4 275 |
| the *second* `git commit` | **0.07 s** | — |

The second commit is fast because Git persists the cache tree it was forced to
build. So the cost was a one-time tax on the first commit of every workspace —
which is exactly the commit the benchmark measures, and exactly the commit an
agent job makes.

A second defect turned "recompute every tree" into "write every tree". `utime()`
on a projected pack returned **EROFS**, and Git's `freshen_packed_object()`
reads a failed freshen as "cannot vouch for this object", so it wrote a loose
duplicate of something the ODB already served. Control test, `hash-object -w` on
a file already in the pack: a native clone wrote **0** objects, the mount wrote
**1**.

## Plan

**Phase 1 — the cache tree** (`crates/gfs-git/`)
* `index.rs`: `CacheTree` (name, tree OID, recursive entry count, children) and
  a `TREE` extension written after the entries and before the trailer.
* `libgit2.rs`: `descend` — the walk that already produces the index entries —
  returns the node for the directory it walked. Its invariants moved into a
  `Walk` struct so the recursion carries only what varies.
* Two guards before serialization: `sort_cache_tree` puts every level in Git's
  order and refuses a duplicated name; `verify_cache_tree` checks every recorded
  count against the entries themselves by range lookup.

**Phase 2 — the freshen** (`crates/gfs-mount/src/fs.rs`)
* `setattr` on an ODB node accepts a times-only request as a no-op and returns
  the current attributes. Mode, size, and owner stay `EROFS`.

**Phase 3 — verification**
* `crates/gfs-test`: a `siblings` fixture whose directory names a Git tree and a
  cache tree sort differently. `FIXTURE_VERSION` bumped to `v3`.
* `crates/gfs-git/tests/repository.rs`: the shipped cache tree is **byte
  identical** to the one `git read-tree` builds, across eight fixtures.
* `crates/gfs-mount/tests/workspace_git.rs`: a one-file commit in a mount writes
  the changed path and not the whole tree, and re-hashing content the projection
  already holds writes nothing.
* Full workspace suite, clippy, and a re-run of `benchmark-workflow.sh`.

Built as planned.

## Decisions

* **Fix the index, not the object writes.** The two defects compound, but only
  one is the cost. Measured on the same mount with the EROFS fault untouched, an
  index that carries a cache tree commits in 0.10 s (django) and 0.206 s
  (vscode) and writes 15 and 25 objects. Fixing the freshen alone would have
  left Git recomputing several thousand trees to discover it already had them.
* **Ship the cache tree rather than let Git build it.** The server already walks
  the whole tree to produce the entries, so the nodes are free; the client would
  otherwise pay for the same walk through FUSE, once per workspace. It is safe
  to ship precisely because the index describes a *commit* — every directory is
  unmodified by construction, so no node is ever invalid.
* **Verify the counts rather than trust the walk.** Git *trusts* a well-formed
  cache tree: a wrong OID or entry count is reused whole and yields a wrong
  commit tree with no diagnostic. That is the one failure this change could
  cause, so `verify_cache_tree` re-derives every count from the entry list —
  a range lookup per directory, cheap enough to pay on every index. A mutation
  that adds one to a single count is caught with the path that is wrong.
* **The oracle is Git, not a re-derivation.** The test compares our extension
  byte for byte against what `git read-tree` writes for the same commit. A test
  that re-implemented the format would share a bug with the writer.
* **Sort by Git's rule, not the walk's.** A Git tree sorts a directory as though
  its name ended in `/`; a cache tree sorts subtrees shorter-name-first, then by
  bytes. Reusing the walk order gives `aa, ab, a-b, a.b, b, c, zzz, a` where Git
  writes `a, b, c, aa, ab, zzz, a-b, a.b`. Live in the corpus: one directory in
  django, seventeen in vscode.
* **`utimensat` on the projection is accepted, not obeyed.** Git touches a pack
  to keep `gc` from pruning it. There is nothing here to prune — the mtimes are
  synthetic and the packs are the server's — so accepting the call is a lie with
  no consequence, and refusing it costs a duplicate object every time Git writes
  something GFS already has. Anything that would really alter the projection is
  still `EROFS`.

## Details

* **Numbers after the fix**, fresh mount, same five-file change, `git commit`
  measured on its own: django **8.06 s → 0.206 s**, 3 254 → 14 objects, 0
  duplicates; vscode **12.34 s → 0.206 s**, 4 299 → 25 objects.
* **In the benchmark**, whose `commit` step is `git add -A` plus `git commit`:
  vscode **14.501 s → 1.953 s**, django **9.705 s → 1.372 s**. The whole task is
  now **4.327 s** on vscode against 11.219 s for a shallow blobless clone, and
  **2.831 s** on django against 2.873 s — GFS is below the cheapest raw clone on
  both. Commit correctness still passes on django with an identical tree; vscode
  still "fails" for the pre-existing reason the report records, which is git-lfs
  deleting 100 files in the *clone*.
* **Local disk and fetched bytes fell with it**: vscode's workspace 22 MB → 3.5
  MB and its object-store traffic 146 MB → 65 MB, because Git no longer reads
  the whole tree through the projection to recompute what it already had.
* **What is left is `git add -A`** — 1.19 s on django, 1.46 s on vscode. That is
  the full-tree `lstat` refresh over FUSE, the same walk as a cold `git status`,
  and a different problem from this one.
* **The index grows** by the extension: django 940 717 → 1 048 385 bytes
  (+11.4 %), vscode 2 615 877 → 2 760 989 (+5.5 %).
* **A mis-sorted cache tree is benign, a mis-counted one is not.** Git's reader
  inserts subtrees into a sorted array, so file order is recovered on read — a
  deliberately reversed extension still produced correct trees. The counts have
  no such recovery, which is why the guard is on them.
* **Gitlinks are entries, not nodes.** A submodule is one index entry of its
  parent and names a tree in another repository; the walk never recurses into
  one, and the `modes` fixture covers it in the byte-identity test.
* **Not done here.** `git add -A`'s refresh sweep, and the fact that the mount
  cannot report `dev`/`ino` an index would match (`core.checkStat=minimal` is
  what makes the seeded stat data work at all).
