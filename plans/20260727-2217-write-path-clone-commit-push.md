# Write path: clone, switch, commit, push

## Summary

The requested flow is:

```
gfs clone https://github.com/pallets/flask.git
cd flask
gfs status
gfs log -10
gfs switch new-branch      # branch created via the gateway, mount re-pinned to it
<edit files>
gfs commit -m <message>    # commit made in the gateway, mount re-pinned to the new commit
gfs push                   # gateway pushes to the real Git server
```

The flow is coherent and it is the natural shape for this project: a workspace with
no object database, where every Git operation is answered by the gateway. Four of
the seven steps are new capability rather than new plumbing, and one of them --
`push` -- is currently refused on purpose rather than merely absent.

**This is a change of premise, not a feature.** GFS today is a read-only
materialization of upstream history plus a local overlay whose only exit is
`gfs export` (a bundle someone else applies). The proposed flow makes the gateway
a write path and the mount a read-write clone. Three ADRs encode the read-only
assumption and have to be revisited rather than worked around:

- **ADR 0002** (Git object authorization boundary) -- reasons about what may be
  *served*; a write path adds what may be *created*.
- **ADR 0005** (Git command surface) -- chose a synthesized `.git` over a real
  partial clone specifically because the workspace cannot produce objects.
- **ADR 0006** (MVP boundary) -- names the read-only gateway as the boundary.

## Where each step stands

| step | status | what is missing |
| --- | --- | --- |
| `gfs clone <url>` | **blocked** | No ingest path. `mirror::fetch` exists and is tested but has **zero production callers**; the only way a repository enters the catalog is `import_repository` in `gfs-server/src/bin/gfs-server.rs`, from a *local path*, with `upstream_url: None`. `upstream_url` is `Some(..)` only in one unit test. No RPC. |
| `cd flask` | **works, given clone** | `clone` must pick the directory from the URL and mount there; `mount` today requires explicit `--repo` and `--workspace`. |
| `gfs status` | **small fix** | The subcommand exists but requires `--workspace`. `rg`, `find` and `log` already discover the workspace from the cwd via `gfs_cli::workspace::resolve`. Point `status` (and `diff`, `inspect`, `health`, `export`) at the same helper. |
| `gfs log -10` | **works today** | -- |
| `gfs switch <branch>` | **blocked** | Two independent gaps: no branch-creation write path (`GitRepository`'s only writer is `create_lease_anchor`, confined to the reserved namespace), and the daemon can only re-pin by re-resolving the *same* selector from its config. |
| edit files | **works today** | Overlay. |
| `gfs commit -m` | **blocked** | No server-side commit construction anywhere, and no state transition that re-pins the mount *and* clears the overlay. `Daemon::refresh` explicitly refuses a non-empty overlay. |
| `gfs push` | **blocked** | The gateway refuses `git-receive-pack` with `PermissionDenied` by explicit match arm. No push-to-upstream. `credential_ref` is a catalog column nothing populates. |

## Technical blockers, deepest first

### 1. The gateway has no write path, by decision

`crates/gfs-service/src/gateway/mod.rs` matches `git-receive-pack` and returns
`PermissionDenied` with "this repository is read-only over Git; push is not
supported". This is not an unimplemented branch; it is a stated boundary. Opening
it reopens ADR 0002's analysis, because "which objects may a caller cause to
exist" is a different question from "which objects may a caller read".

Note that `gfs push` does **not** require receive-pack on the gateway. The gateway
pushes *outward* to the real Git server as a subprocess, exactly as `mirror::fetch`
already pulls inward. Receive-pack would only be needed to let stock `git push`
target the gateway, which this flow does not ask for. Keeping it refused is
compatible with everything below.

### 2. `gfs-git` cannot write objects

The `GitRepository` trait is read-only apart from lease anchors. There is no
`write_tree`, `create_commit`, or general `update_ref`. This is buildable --
`git2` is built with `default-features = false`, which drops only the *network*
features (`https`, `ssh`), so local object writing is fully available -- but it is
new surface with real correctness stakes, and it must go through the same
repository lock the rest of the ref/maintenance path uses.

### 3. The overlay-to-commit transition does not exist as a concept

This is the actual hard part, more than the object writing. `gfs commit` has to:

1. build a tree from the base commit plus the overlay journal,
2. create a commit object,
3. move the branch ref,
4. prepare the snapshot so search and `log` work on it,
5. re-pin the mount to the new commit,
6. clear the overlay, because its changes are now in history.

Steps 5 and 6 must not be separately observable. A crash between them leaves
either an overlay claiming changes that are already committed (they would be
re-applied and double-counted) or a mount pinned to a commit whose overlay was
discarded. `Daemon::refresh` already publishes generations atomically and retires
the old one only when its open handles drain -- that machinery is the right
foundation, but its precondition is an *empty* overlay, which is the opposite of
the commit case.

### 4. Ingest is unwired

`mirror::fetch` runs only in tests. `gfs clone` needs: create a bare mirror
(`git clone --bare` or `init --bare` + fetch), `create_repository` with
`upstream_url: Some(..)`, `registry.activate` (which applies ADR 0001's format
gate), then the existing `CreateMount` path. Every piece exists except the first
and the RPC that sequences them.

### 5. Push credentials and identity

`credential_ref` is a schema column with no store behind it, and `mirror::fetch`
takes an `Option<&str>` credential that the comment marks as M1-incomplete. Push
raises a question fetch does not: **whose identity does the push carry?** A
per-repository server credential means every user's commits reach upstream as the
service account, and upstream branch protection sees one actor. A per-user
credential means the gateway holds user tokens. This is a policy decision, not an
implementation detail, and it should be settled before the code is written.

## The verified trap: a new branch under `refs/heads/` is deleted by the next fetch

`mirror::fetch` runs `git fetch --prune --prune-tags` with

```
FETCH_REFSPECS = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]
```

So any branch created locally under `refs/heads/` that upstream does not have is
**pruned on the next mirror fetch** -- silently, as routine maintenance. The
existing tests prove the prune is real
(`a_fetch_with_explicit_refspecs_prunes_upstream_deletions_but_not_anchors`).

`gfs switch new-branch` creating `refs/heads/new-branch` in the mirror would
therefore work until the next fetch and then lose the branch, and with it the
reachability of any commit made on it.

**Proposed shape.** Unpushed work lives in the reserved namespace, e.g.
`refs/gfs/work/<subject>/<branch>`, which:

- is never a fetch destination, so prune cannot reach it (this is the same
  property that protects lease anchors, and `refspec_is_safe` enforces it);
- is already hidden from advertisement by `transfer.hideRefs=refs/gfs/`, so
  unpushed work is not exposed to gateway clients;
- keeps commits reachable, so `git gc` cannot prune them.

`gfs push` then maps `refs/gfs/work/<subject>/<branch>` to `refs/heads/<branch>`
on the upstream explicitly. After a successful push the branch also arrives
through the normal fetch path, and the work ref can be retired.

## Semantic questions that need a decision first

1. **Where do user branches live?** Recommendation: the reserved namespace, per
   above. The alternative -- `refs/heads/` plus a prune exclusion -- weakens the
   invariant that ADR 0006 relies on.
2. **Is a `gfs commit` a real commit immediately?** Recommendation: yes, created
   server-side at commit time. The alternative (accumulate and materialize at
   push) means `gfs log` cannot show local commits, which breaks the flow's own
   premise. Consequence to accept: there is no "local-only" commit -- every
   commit is already on the gateway, so a failed `push` leaves commits the
   upstream does not have. That is a *feature* for durability and a surprise for
   anyone expecting `git commit`'s locality; it should be stated in the docs.
3. **Whose credential does `push` use?** Blocking for `push` only; everything
   through `commit` can be built without answering it.
4. **What does `gfs switch` do with a dirty overlay?** `git switch` carries
   changes when it can and refuses when it cannot. `refresh` currently refuses
   outright. Recommendation: refuse in the first cut, matching `refresh`, and
   say so in the error.

## Suggested sequencing

Each phase is independently useful and independently testable. AGENTS.md requires
compiling and running tests after each.

- **Phase 0 -- cwd discovery.** Make `status`, `diff`, `inspect`, `health`,
  `export` accept an optional `--workspace` and fall back to `workspace::resolve`.
  No new concepts; makes `cd flask && gfs status` work. Small.
- **Phase 1 -- `gfs clone`.** Ingest RPC: create the bare mirror, fetch, register
  with `upstream_url`, activate, then mount at a directory derived from the URL.
  Delivers the first two lines of the flow with no write path at all.
- **Phase 2 -- object writing in `gfs-git`.** `write_tree`, `create_commit`,
  `update_ref` on the trait and the libgit2 implementation, under the repository
  lock, with the reserved-namespace rule enforced for work refs.
- **Phase 3 -- `gfs switch`.** Branch creation in the reserved namespace plus a
  daemon request that re-pins to a *different* revision. Extends `refresh` rather
  than duplicating it.
- **Phase 4 -- `gfs commit`.** The overlay-to-commit transition, including the
  atomic re-pin-and-clear. The riskiest phase; it deserves its own crash tests
  alongside the existing overlay crash harness.
- **Phase 5 -- `gfs push`.** Outbound push as a subprocess, mirroring
  `mirror::fetch`'s sandbox, plus the credential decision from question 3.

## Plan

Six phases. **0 through 3 are built, tested, and passing `scripts/check.sh`.
4 and 5 are not started.**

- **Phase 0 -- cwd discovery. DONE.** `--workspace` is optional on `status`,
  `diff`, `inspect`, `health`, `export`, `refresh`, `unmount` and `search`,
  falling back to `gfs_cli::workspace::locate`. It stays *required* on `mount`,
  where it is an output path rather than a lookup, and stays available
  everywhere else because `scripts/dev-stack.sh` and any orchestrator drive a
  mount from outside it.
- **Phase 1 -- `gfs clone`. DONE.** `gfs-service::ingest` sequences
  `init_bare` + `fetch` + `create_repository` + `activate`; the
  `RepositoryService.CloneRepository` RPC exposes it; `gfs clone <url> [dir]`
  takes `git clone`'s arguments and mounts the result. The server enables it
  with `--repos-root`, and refuses every write method without one.
- **Phase 2 -- object writing. DONE.** `write_tree`, `create_commit`,
  `update_work_ref` and `read_ref` on `GitRepository` and its libgit2
  implementation, plus async wrappers. Verified against **stock Git** rather
  than by reading back through libgit2.
- **Phase 3 -- `gfs switch`. DONE.** `RepositoryService.CreateBranch` creates
  `refs/gfs/work/<subject>/<branch>`; `Request::Switch` re-points the view by
  resolving a different selector through the existing generation machinery.
- **Phase 4 -- `gfs commit`. NOT STARTED.** See "What Phase 4 still needs".
- **Phase 5 -- `gfs push`. NOT STARTED.** The proto message and a stub handler
  exist; the `git push` subprocess and the CLI command do not.

## Decisions

**The user's credential, per call, never stored.** Answering question 3: push
carries the caller's own credential the way raw `git` does, so upstream sees the
user and not the service. `PushBranchRequest.credential` is per-call and the
catalog's `credential_ref` column stays unused by this path.

**There is no local-only commit, and that is the model rather than a
limitation.** The gateway's mirror *is* the clone; a mount is a view onto it,
the way `git worktree` is a view onto a repository, except that a view
materializes no working files. That is what makes thousands of views over one
mirror affordable. Consequences accepted deliberately:

* every commit is durable on the gateway the moment it is made, so a failed
  `push` leaves commits upstream does not have;
* `gfs log` can show local commits, which the alternative (accumulate and
  materialize at push) could not;
* two views of one mirror can commit to one work branch concurrently, which is
  why `update_work_ref` is a compare-and-swap and the loser is told to retry
  rather than silently clobbering.

**Unpushed work lives in `refs/gfs/work/<subject>/<branch>`.** Answering
question 1. `refs/heads/` was rejected because `mirror::fetch` runs
`--prune --prune-tags` over `+refs/heads/*:refs/heads/*`, so a local branch
upstream does not have is deleted by the next sync. `update_work_ref` refuses
any name outside the reserved namespace, so this cannot be got wrong by a
caller.

**`switch` re-pins to a resolved commit, not to a branch name.** ADR 0006
forbids naming `refs/gfs/` as a revision, so the CLI resolves the branch through
the gateway and sends the commit; the branch name travels alongside purely so
reports can name it. This kept the reserved-namespace rule intact instead of
carving an exception into the selector grammar.

**The repository id is derived from the URL, the directory is not.**
`github.com_pallets_flask` keeps two organisations' `flask` apart, which a bare
`flask` would not; the mount directory is `flask`, because that is what
`git clone` makes.

## What Phase 4 still needs

`gfs commit` is the keystone and the riskiest step, and it is untouched. What it
needs, in the order it has to happen:

1. **The overlay's changes, with content.** `Overlay::status` reports six kinds
   -- `Added`, `Modified`, `Deleted`, `TypeChanged`, `ModeChanged`, `Renamed` --
   and `FileChange` in the proto carries three. The extra three each need a
   decision: a rename is a delete plus an add to a tree, a mode change is an
   upsert whose content is unchanged, and a type change is a delete plus an add.
   `Overlay::open_content` supplies the bytes.
2. **`RepositoryService.CommitChanges`.** Build `Vec<TreeChange>`, then
   `write_tree` on the base, `create_commit`, and `update_work_ref` with the
   base as the expected value so a concurrent committer loses rather than
   clobbers. Content is inline in the request today, which bounds a commit by
   the gRPC message limit -- acceptable for a prototype, and the server must
   *say so* rather than truncating.
3. **`Daemon::commit_to`: the atomic re-pin and clear.** The part that needs
   care. Re-pinning and emptying the overlay must not be separately observable:
   a crash between them either re-applies changes that are already committed or
   discards work that is not. The generation machinery is the right foundation
   -- publish first, retire as handles drain -- but ordering the overlay reset
   against the publication is a new question, and it deserves tests alongside
   the existing `gfs-overlay-crash` harness.
4. **`gfs commit -m`** in the CLI, reading the work branch the daemon recorded
   at `switch` time.

Phase 5 is much smaller: a `git push` subprocess mirroring `mirror::fetch`'s
sandbox, mapping `refs/gfs/work/<subject>/<branch>` to `refs/heads/<branch>`,
plus a `gfs push` command. The proto message and a stub handler already exist.

## A consequence worth knowing about `switch`

After `gfs switch`, a shell whose cwd is inside the workspace gets `ENOENT` on
the next command. The workspace is a symlink into
`<state>/generations/<n>`, and a shell's cwd resolves through it once: when the
switch publishes generation `n+1` and retires `n`, the shell is still standing
in the old one. `cd $(pwd)` recovers.

This is inherent to the generation model, which PLAN.md M2.1 requires -- a
refresh must never mutate the pinned base under existing kernel dentries. `git
switch` has no equivalent problem because it mutates in place. Worth either
documenting or having the CLI print a hint; it will affect `gfs commit` too.

## Details

Findings recorded during investigation, with the file each came from:

- `crates/gfs-service/src/gateway/mod.rs` -- `git-receive-pack` is refused by an
  explicit match arm, not missing.
- `crates/gfs-service/src/mirror.rs` -- `fetch` is tested but has no production
  caller; `FETCH_REFSPECS` with `--prune` is what makes a local `refs/heads/`
  branch unsafe.
- `crates/gfs-git/src/repository.rs` -- the trait's only writer is
  `create_lease_anchor`, and it refuses anything outside the reserved namespace.
- `crates/gfs-mount/src/daemon.rs` -- `refresh` builds a new generation and
  publishes it before retiring the old, which is the mechanism `switch` and
  `commit` should reuse; its empty-overlay precondition is what `commit` must
  replace rather than satisfy.
- `gfs-server/src/bin/gfs-server.rs` -- `import_repository` is the only path into
  the catalog, and it takes a local path.
- `gfs-cli/src/workspace.rs` -- `resolve` already walks up from the cwd; only the
  newer subcommands use it.
